# A11 — ADR proposals and the incremental PR plan (#407)

**Status:** capstone of the #395 planning tree · **Date:** 2026-08-25 ·
**Tree:** `docs/407-a11` (based on `main` at `2b542c4d`) · **Parents:**
[#407](https://github.com/baadc0de/orrery/issues/407) ←
[#395](https://github.com/baadc0de/orrery/issues/395) · **Consumes:** all of
[A1](a1-ruleset-architecture-map.md)–[A9](a9-engine-boundaries.md) on `main`,
plus [A10](a10-conformance-benchmarks.md) read from PR #423
(`origin/docs/406-a10`, still open) · **Source brief:**
[docs/plans/ruleset-ecs-migration-brief.md](ruleset-ecs-migration-brief.md)

Ten nodes produced evidence, decisions-as-proposals, formats and fixtures.
This document turns them into the four artifacts the epic's acceptance
demands: the **ADR set** (proposals — accepting any of them is the owner's,
always), the **twenty owner decisions** each with an explicit disposition,
the **phased PR plan** with compatibility adapters and a rollback strategy
for the migration itself, and the **traceability table** proving no section
of the brief was silently dropped.

Method, as in every predecessor:

- Every load-bearing claim was re-verified on this tree before use; §10
  records the verification set and the one first-hand mutation
  (break stage → named check dies → revert → passes). This branch is
  docs-only over `2b542c4d`: `git diff --stat 2b542c4d HEAD -- crates gates
  scripts vendor` is empty, so every predecessor mutation log (A1 M1–M8,
  A2 M-A/M-B, A3 F-1/F-2 + P1–P5, A4 M-G1 + probes, A5 X1–X5,
  A6 M-A6-1..4b, A7 X-A..X-E, A8 M-A8-1..3, A9 M1–M3, A10 R-1..R-5)
  carries over re-based at full strength.
- What **is settled by an accepted record**, what is **proposed and awaits
  the owner**, and what is **deferred with a named condition** never share a
  sentence.
- This is planning, not implementation. Implementation issues are created
  only after the owner accepts the relevant ADRs (#395 header), and nothing
  below schedules digest-crate work before P4 exit (§5.1).

---

## 1. The decision this plan is built around

Adopted position, reached independently by two A3 lanes and consumed as the
architecture by A4–A10 (a3-simulation-host-comparison.md §7;
a3-simulation-host-second-opinion.md §6):

1. **Canonical verifiable state stays in the engine-neutral per-entity
   executor** (`Executor`'s `BTreeMap<PersistId, R::CoreState>`,
   `crates/orrery_core/src/executor.rs:48-51`). The dedicated-store topology
   already ships; it is the boundary (A9 B-1).
2. **The composition root (brief phase 2) and the `SimulationHost` seam
   (brief phase 3) land now** — both lanes note these are
   variant-independent and pay for themselves under every future.
3. **The shared Bevy application world is rejected outright** (268/500 and
   62/165 in two independent matrices; it converts every structural
   guarantee — witness projection, rollback scope, backend neutrality —
   into review-held convention).
4. **A dedicated `bevy_ecs::World` is admitted only behind the host seam,
   on pre-registered triggers** (T1: `CoreState`-as-one-enum measurably
   stops scaling under per-component policies; T2: measured tick cost shows
   the store dominating; T3: A4's Tier-H gate bundle lands and is
   demonstrated at least as strong as the clauses it replaces). Until a
   trigger fires, the gated crates stay Bevy-free and `core-gates.sh`
   clause 1 stays exactly as it is — proven live on this tree today
   (§10 M-A11-1).

Everything in §2–§8 is sequenced under that position. The pre-registered
reversal condition also carries: if A10's E-1-class experiment shows the
composition root cannot hold modularity at second-game scale, the pivot is
the hybrid tier model (A3 H1/V5), not the shared world — and the pilot
preconditions (A4 Tier H, A5 policy registry, A10 G-3 harness, capacity
mirror numbers) stay binding regardless.

## 2. The ADR set — proposals, deduplicated

Ten nodes flagged ADR-worthy decisions. Collected, deduplicated, and
grouped into **eight proposed records**, three **document amendments**, and
one **programme acceptance** that is deliberately not an ADR. For each
record: what it decides, what it amends or extends, which node's text
supplies the substance, and why it is a separate record rather than a
clause of another.

| # | Proposed record | Decides | Amends / extends | Substance | Separate because |
|---|---|---|---|---|---|
| **R1** | **Canonical simulation architecture** | §1's four points verbatim: executor-hosted canonical state; composition root + `SimulationHost` seam; shared app world rejected; dedicated world trigger-gated (T1–T3). Absorbs A9's B-1 ("canonical truth never lives in a Bevy application world") — same rule, stated once | Extends D15's layering prose (docs/10-crates.md rules) with the host-seam layer; names no frozen surface; does **not** reopen D21 (no trait change) | A3 §7 + second opinion §3 V5/§6; A9 §2.1 | The umbrella every other record assumes; accepting or refusing it re-scopes all of R2–R8 |
| **R2** | **Determinism envelope and gate replacement** | The three-ring envelope (in-process bit-exact; matrix discrete-exact/banded; outside-envelope list); canonical stages S0–S7 with S4≺S5 non-negotiable; Tier V role-discovery gate replacing the typed `GATED_CRATES` list; conditional Tier H clause battery (arms only with an R1 trigger); schedule digest's existence and content; overflow policy (owner picks the constant posture, A4 §11.4) | Amends the enforcement mechanism of D9/D15's Bevy-free property (`scripts/core-gates.sh`); strength accounting in A4 §5.3 (equal in kind, strictly stronger at the edges; the honest caveat about Tier H recorded there) | A4 §3–§6 | It changes a live gate; the epic's standing rule ("a weaker gate that passes is worse") gives it its own acceptance bar, and its Tier-H half has a different cadence (conditional) than R1's |
| **R3** | **Identity classes and allocation** | Three closed identity classes (N-1); host-owned `PersistId ↔ Entity` index rules (N-2); granted-range derivation composing paths 1+3 of allocation (N-3 — closes gap G-2); no provisional durable identity (N-4); `(GridId, PersistId)` rider for cross-grid references; id-reuse-after-despawn policy (owner's trade, both options priced) | Extends docs/08 §6 vocabulary (block grants); corrects `persist.rs:41-44`'s present-tense description of the unbuilt grant path | A5 §2 | Its two open owner calls (N-3, reuse) can be deferred without blocking R4; and it touches persistence vocabulary none of the others do |
| **R4** | **Per-component capability policy** | Schema pair `(ComponentTypeId, SchemaVersion)` as the id of record (N-5); reflection-never-defines-encodings (N-6); five independent dimensions P/R/W/N/A, zeros fail closed (N-7); the eight invalid combinations IV-1..IV-8; `classify_component` **replaced** (not wired) with removal sequenced last; `CoreClass` survives as derived vocabulary only; the tier predicate becomes `W2`; IV-7's enforcement direction (`EngineHandleFree` compile-time bound or registry-time schema refusal — A9 §3, mechanism the owner's) | Overwrites docs/06 §2's present-tense `classify_component` consumer prose; the eventual removal of a *defaulted* trait method is not D38's "required method" branch but is flagged as trait-surface change landing at the owner's pleasure, post-P4-digest | A5 §4–§6; A9 §3 | It defines the registry R8's manifest consumes and retires a trait member — reviewable on its own terms; folding it into R8 would let manifest formatting smuggle a trait change |
| **R5** | **Message-class semantics** | Six classes with internal commands mechanically collapsed onto domain events; C-1 (external dedup only below the seam or by durable op-id); the R1–R7 immediate-vs-stage-delimited rules; delivered-first input composition **ratified as law** (changing it costs a `RulesetId` bump); C-2 volume bounds (per-step emission cap, fail-loud vs flag posture and constants owner-set) | Ratifies existing convention (`scenario.rs:209-214`) into normative text; C-2 introduces a canonical error path that does not exist today — the reason this is a record, not a refactor | A6 §2–§4, §9 | Ratification of delivered-first has rules-version implications; the overflow posture is an owner choice with its own alternatives section |
| **R6** | **Rollback unit** | R-1: unit = per-entity predicted set, grain = R1-component subset with all-or-nothing restore at witnessed entities, window = the 9-tick ring, budget = the existing ladder; world/island/cell rejected with arguments; canonical = correction-only, durable = recovery-only, critical = compensation-only; L-1: lightyear history is presentation-tier, one door (`AuthorityCorrectionInbox`) | Annexes/promotes docs/05 and docs/06:521 prose (straight-line authority log) into a decision record adjacent to D8 | A7 §2–§3 | A7 proposed it as its own record; it binds the pilot's rollback substrate and can be accepted independently of the projection format |
| **R7** | **Canonical witness projection** | WP-1..WP-6: entity-tick commitment unit; `PersistId`-ascending (cross-grid `(GridId, PersistId)`) ordering for any hashed enumeration; `(ComponentTypeId, SchemaVersion, payload)` slot framing ≡ persistence framing; quantize-before-hash (currently unpinned — X-C — closed by the first PR, §7); no engine artifact in the projection; **`projection_version` as a third orthogonal version axis** | **Amends D38's version-domain law** (clause (d)(3), docs/adr/0038:198-205): schema ⊥ rules gains projection ⊥ both (A7 M-4). This is the one proposal that edits an accepted record's normative content | A7 §5, §7.1 | It amends D38; R6 does not. Separate acceptance keeps the D38 amendment reviewable on its own |
| **R8** | **Compatibility manifest** | The field set (keep/reshape/reject verdicts per A8 §2, incl. rejecting `command_schema_id` and the canonical-config hash, reshaping `kernel_version` to a toolchain stamp and `determinism profile` to `profile_id`); X-1: `RulesetId.digest` becomes blake3 over the determinism-relevant source closure (scope decided, mechanism owner's); X-2: permanent build-keyed manifest record in the cluster keyspace; X-3/X-4: `removed` list + no silent read-and-drop; X-5: reviewed per-game `ComponentTypeId` registry file; the seven-axis composition law (§9.1); rolling upgrade: **no general story, by D29 clause 5, reaffirmed**; static composition **ratifying** D21 | Extends D21 through its additive door (new keyspace family, new types); generalizes D38(d)(3)'s orthogonality sentence from two axes to seven; reaffirms D29 clause 5 without reopening it | A8 §2–§9 | A8's own proposal: "one new ADR … amending neither record's accepted text". The schedule digest's *storage* lands here; its *content* is R2's (the A4/A8 division kept) |

**Deduplication notes — what was collected and merged rather than
multiplied:**

- A9's B-1 and A3's V2-rejection are one decision; R1 states it once.
- A9's `EngineHandleFree` proposal is IV-7's enforcement and lives in R4,
  not as a ninth record — the invariant and its guard belong together.
- The schedule digest appears in A4 (content) and A8 (storage); R2/R8 split
  it exactly as the nodes did, and the *wire placement* question (assert at
  session setup?) is deliberately in neither record — it is owner decision
  OD-24 (§3.2), because both nodes flagged it as protocol-adjacent.
- A7's G-1..G-3 (outcome chains, differential harness) and A10's fixture
  set are **not ADRs**: they are test infrastructure, accepted as the
  programme item below, unless the owner elevates the one genuinely
  protocol-shaped door (event commitments entering `StateClaim` — OD-27,
  recorded as a door, not proposed).
- A6's dedup/idempotency rules and A4's stage timing are one semantic
  package presented as R5+R2 with the boundary the two nodes drew (A4 fixed
  ordering/timing, A6 the rest); no rule appears in both.

**Document amendments (not ADRs, no owner-acceptance gate beyond review):**

| # | Amendment | Content |
|---|---|---|
| DA-1 | `docs/10-crates.md` census | #414 (`orrery_field_host` does not exist — `:29`, `:95`); A9 D-2 (`orrery_aeronet_iroh` listed under `crates/` at `:18`, lives in `vendor/`); A9 D-3 ("all thirteen `orrery_*` crates" at `:3` — fifteen exist; the reference table omits `orrery_conformance`). All three re-verified on this tree today |
| DA-2 | `docs/adr/0021` context parenthetical | "`Ruleset::validate_intent` behind `intent::IntentValidator`" (`:20-21`) describes a trait member that was never implemented (A1 §6); a dated footnote, not an edit of the accepted decision |
| DA-3 | docs/06 §2 + `:210` | Present-tense `classify_component` consumers that never existed; rewritten alongside R4's acceptance (the section R4 overwrites) |

**Programme acceptance (owner sign-off through this node, ADR-light):**
A10's fixture set F-2..F-12 with formats as specified; the threshold table
(A10 §8.4 — proposals the owner tightens or loosens); the
baseline-refusal rule (no differential run without a committed baseline);
and the ordering constraint **F-2 lands before any Phase 2 composition PR**
(outcome goldens must commit *legacy* behaviour). §5 sequences all of it.

---

## 3. The decisions reserved to the owner

### 3.1 The brief's twenty questions, each with a disposition

The source brief carries twenty "questions requiring explicit decisions"
(`ruleset-ecs-migration-brief.md:898-921`). Every one lands here as
**answered** (settled by evidence a node produced; the owner ratifies by
accepting the named record), **settled** (already decided by an accepted
ADR; nothing new to accept), **proposed** (a node produced the answer as a
proposal; it binds only on acceptance), or **deferred** (a named condition
must arrive first). None is withdrawn. Tally: 9 answered · 2 settled ·
7 proposed · 2 deferred.

| # | Question (brief line) | Disposition | Where settled / what decides it |
|---|---|---|---|
| 1 | Is `Ruleset` solving a real extensibility problem or mainly a test seam? (:902) | **Answered** | Real but narrow: the trait is the adjudication/replay seam (A1 §3, §5.2), while the feared generic infection is measured absent — four crates, erased at one boxed closure (A1 §4.4; A3 C1/C-1, both lanes). Monolith pressure at scale is unevidenced; A10's E-1-class experiment is the pre-registered test, and R1's reversal condition keys off it |
| 2 | Which behaviours are game-defined? (:903) | **Answered** | A2 §2 rows 9–10 and the supplementary rows: gameplay components/systems, event vocabulary, invariant predicates, codecs, population budgets, boundary responses. Ratified with R1 |
| 3 | Which behaviours stay in the kernel? (:904) | **Answered** | A2 §2 rows 1–8, 11–15: time, identity classes, authority, transaction envelope, spatial attention, persistence coordination, rollback mechanism, witness pipeline, RNG partitioning, input ordering, event transport, version identity |
| 4 | Does canonical game state already live in a Bevy world? (:905) | **Answered** | No — factually. `Executor` maps outside every app world; every host mirrors (A3 E-1/second opinion §2; A9 E-1/E-6). The brief's framing was inverted: the dedicated topology ships |
| 5 | Can the current Lightyear integration run against a dedicated canonical world? (:906) | **Answered** | It already does, in effect: lightyear/replicon operate on mirror components and never touch canonical state (second opinion E-4, C-2); a mirror hop is required and measured cheap (P-1/P4: µs-scale); lightyear's own per-entity authority is non-functional in 0.29 (`lightyear_replication-0.29.0/src/lib.rs:67-68`) |
| 6 | What is the required determinism envelope? (:907) | **Proposed → R2** | A4 §3.1's three rings: in-process bit-exact; four-target matrix with discrete bit-exact and continuous under D16 bands; pinned-toolchain/tamper/fast-math explicitly outside |
| 7 | What is the rollback unit? (:908) | **Proposed → R6** | A7 R-1: per-entity predicted set, R1-component grain, all-or-nothing at witnessed entities; world/island/cell rejected with arguments; canonical state is never rewound anywhere |
| 8 | Which component categories are persisted, replicated, rolled back, witnessed? (:909) | **Proposed → R4** | A5 §5.4's named profiles (Core, Bulk, Cosmetic-local, Ephemeral-shared, Critical/ledger) over the five dimensions |
| 9 | Can these policies differ independently? (:910) | **Answered** | Yes — the tree already contains the witnesses: W2∧N0 (witness channel ≠ replication), N1∧P0 (projectiles), P1∧W1∧¬W2 (bulk), P2∧R0 (ledger) (A5 §5.3). One flag cannot carry it; hence R4's five axes |
| 10 | How are schema IDs allocated and governed? (:911) | **Proposed → R8 (X-5)** | Reviewed, permanent, per-game registry file; monotone, never reused; duplicate-refusal at composition time. Today Regolith hardcodes `ComponentTypeId(1)` with no registry |
| 11 | How are system ordering and module dependencies validated? (:912) | **Proposed → R2 + programme F-10** | Explicit edges, ambiguity rejected at Error with a canary mutant proving the rejector awake (A4 E-M2); missing/cyclic/duplicate declarations refuse composition (A10 §7.1); schedule digest pins topology |
| 12 | Should module composition be compile-time only? (:913) | **Settled (D21) + ratified → R8** | Yes. D21 is Accepted: link-time distribution, no WASM, no dynamic loading (docs/adr/0021:40-42). A8 §8 adds the manifest consequence (manifests describe builds, not deployments) |
| 13 | Must games add modules without recompiling Orrery? (:914) | **Settled (D21)** | No, for 1.0. D21's reopen conditions (`:85-90`) are recorded and untouched; nothing in this tree fires them |
| 14 | How are rules and manifests versioned in persisted universes? (:915) | **Proposed → R8** | Rows decode by their own slot statements (existing, fail-closed); the permanent build-keyed manifest record (X-2) is the decoder ring; R-1..R-6 of A8 §5 |
| 15 | How do old replays select compatible rules? (:916) | **Answered (exists) + extended → R8** | `RulesetId` routing against `RETAINED_BUILDS = 3`; older evidence resolves `Unadjudicable(UnknownRuleset)`, never a strike (A8 I4/I10/I12). X-2 extends the id with its manifest |
| 16 | Does the Bevy client share the canonical world or mirror it? (:917) | **Proposed → R1** | Mirror — permanently. The A3 question decided head-on, twice independently; the shared world is the rejected variant |
| 17 | What must an Unreal client be allowed to predict locally? (:918) | **Deferred** | No Unreal code, consumer, or owner-stated latency/prediction requirement exists in-tree (A9 §0; A1 assumption 10 unverifiable). The A9 §5 observer proof deliberately predicts *nothing*; the prediction question opens when the owner supplies a concrete Unreal requirement. Named condition: that requirement document |
| 18 | Which presentation events must be reversible after rollback? (:919) | **Answered** | None. Presentation is discarded and regenerated from corrected canonical state; overwrite semantics; no undo logic exists or is permitted (A6 §3.5, R6/L-1) |
| 19 | How are services kept independent from ECS implementation details? (:920) | **Answered (exists) + strengthened → R2** | persistd links zero Bevy (witness engine `default-features = false`); adjudication consumes bundles, never worlds. The strengthening: today's guarantee is a typed crate list (witness carries 530 bevy refs past a green gate); Tier V discovery makes the coverage a property instead of a decision |
| 20 | What is the smallest vertical slice that tests the entire architecture? (:921) | **Deferred (conditional)** | Under R1 the ECS vertical slice is trigger-gated, so the brief's Phase-4 slice question defers with it; its precondition package is already fixed (Tier H bundle + G-3 differential harness + A5 registry + capacity mirror numbers — A3 §7.4). The *current* architecture's smallest full-stack exercise is Phase 2 composition + host seam + the A9 §5 observer proof, each with named falsifiable acceptance |

### 3.2 Decisions the nodes surfaced beyond the twenty

Each was flagged by a node as the owner's and none may vanish into the
plan. Numbered OD-21+ so the traceability table can cite them.

| # | Decision | Raised by | Options priced | Blocking? |
|---|---|---|---|---|
| OD-21 | **F-1 disposition**: `DiffUplink.tick` is documented as the universe tick (`gateway.rs:377-378`) but the only production writer stamps a client-local per-entity sequence from 0 (`feed.rs:81-92`). Fix the writer (stamp real ticks — changes journaled bytes' meaning) or fix the doc (rename the semantic) | A7 §1.1/§9.5 | Both small; silence is the only wrong option. `orrery_persist_client` is outside the P4 digest, so either lands window-safe | Blocks any design assuming a tick-addressed journal (journal/claim alignment, §4); blocks nothing else |
| OD-22 | **X-1 mechanism**: how `RulesetId.digest` gets computed (build script / CI artifact / lazy runtime) — the scope is decided in R8; stale artifacts are worse than honest placeholders | A8 §13.2 | Unpriced by design; choose with costing | Blocks the digest carrying information in the differential harness (until then the harness keys off version fields only, A10 §4.3) |
| OD-23 | **Overflow policy** for canonical integer math: `overflow-checks = true` in all profiles (recommended) vs explicit `wrapping_*` | A4 §11.4 | Either works; silence splits dev/release (P-OV demonstrated the wrap/panic split on this tree) | Blocks the profile-parity matrix leg (E-M8) — the leg must land *with* the policy or it pins today's accident |
| OD-24 | **Schedule-digest / `projection_version` session assertion**: whether log-exchanging parties assert them at session setup (wire-adjacent) or manifest-only | A4 §11.3; A8 §10.3 | Narrowest proposal in A8 §4; widening the handshake reopens nothing but adds surface | Non-blocking; out-of-band assertion suffices meanwhile |
| OD-25 | **N-3 granted-range derivation** (closes G-2's unpartitioned u64 space) and **id-reuse-after-despawn** policy | A5 §2.4/§2.6 | N-3 vs static high-bit partition (rejected as proposal — collides across emitters); reuse-forbidden has retention costs not priced | Blocks *persisting materialized entities* (phase-5-class work); latent until then |
| OD-26 | **G-1 mechanism**: `EngineHandleFree` sealed bound at the replicon registration seam vs registry-time schema refusal (byte-scanning ruled out — entity bits are indistinguishable from any u64) | A9 §3 | Compile-time bound is nearer-term; registry refusal is the durable form; both can land | Blocks F-9; a prerequisite before any capability registry ships (§4) |
| OD-27 | **Event commitments entering `StateClaim`** — the only door that would close the *adjudication* gap for event-only outcomes (fixtures close the parity gap) | A7 §5.3/§12.3 | Protocol change; claim size; A6 interplay — deliberately unpriced | Non-blocking; recorded as a door, not proposed |
| OD-28 | **C-2 constants and posture**: emission cap value; fail-loud canonical error vs stage-1-style flag | A6 §9/§12.2 | Both deterministic; the error path is new surface | Blocks only the volume-bound clause of R5 |
| OD-29 | **X-4 quarantine override**: whether a read-and-quarantine operator tool ships at v1 | A8 §10.5 | Forensics value vs a laundering hazard; default (refuse) ships regardless | Non-blocking |
| OD-30 | **The three unused `bevy_reflect` Cargo entries** (`orrery_spatial`, `orrery_net`, `orrery_persist_client`) — needed for vendored-replicon feature unification, or dead weight? | A5 §5.1/§11.4; A9 E-8 | One `cargo tree`/build experiment answers it; not attempted on docs-only branches | Non-blocking |
| OD-31 | **#417's closure shape**: pin the shadowed `LocallyAuthoritative` clause with the F-7 fixture, or collapse the redundancy if `LocalGranted`-without-marker proves unreachable | #417; A5 X2; A10 §13.2 | The fixture PR carries the reachability determination | Non-blocking; window-safe |
| OD-32 | **A10 threshold numbers** (§8.4) and baseline placement (in-repo vs release artifact) | A10 §13.1/§13.6 | Ratios proposed from the frame budget; revise at first capture | Blocks phase-exit evaluation semantics only |

---

## 4. The open findings, classified

Every live finding the tree carries, sorted into **prerequisite of the
plan** (something in §5 cannot ship until it closes), **owner-first**
(nothing can be scheduled until the owner disposes of it), and
**independent** (real, tracked, closable on its own cadence). A plan that
quietly absorbed any of these would be worse than one that lists them; none
is absorbed.

| Finding | Status on this tree | Class | Disposition in this plan |
|---|---|---|---|
| **#414 + A9 D-2/D-3** — `docs/10-crates.md` census wrong three ways: names `orrery_field_host` (does not exist, `:29`, `:95`), places `orrery_aeronet_iroh` under `crates/` (`:18`; it lives in `vendor/`), says "thirteen crates" (`:3`; fifteen exist, `orrery_conformance` omitted from the reference table) | All three re-verified today; #414 open | **Independent** | DA-1 / PR-0 — needs no ADR, no acceptance gate, lands any time |
| **#417** — `feed_uplink`'s `LocallyAuthoritative` guard unpinned: deleting it leaves all 95 tests green (the fixture entity is refused by a different clause first) | Open; reproduced by A10 R-4 | **Independent**, window-safe | PR-3 (F-7 fixture) carries OD-31's reachability call inside the PR |
| **A5 G-1 / A9 M3** — no mechanical guard against engine handles in replicated payloads; `entity.to_bits()` rides a `DiffUplink` into the journal past every gate and 100 tests | Demonstrated twice (A9 M3, A10 R-3) | **Prerequisite** for the capability registry and any ECS pilot; **owner-first** on mechanism (OD-26) | PR-8 after the owner picks; F-9 pins it; until then the gap stays open and listed — no byte-scanning theatre |
| **A5 G-2** — derived and cluster-minted ids share an unpartitioned u64 space; a persisted materialized entity could silently collide with a minted row | Latent (materialized entities are not persisted today) | **Owner-first** (OD-25 / N-3) | Blocks persisting materialized entities (phase-5-class); nothing in tranches 0–2 touches it |
| **A5 G-3** — the bulk uplink makes no schema statement; diff-overwritten bags reset floors to v0; the framed `ComponentBag` has no production writer | Re-verified by A8 I8 | **Prerequisite** for wiring the P capability dimension | The framed-bag producer package rides the Phase-2+ implementation epic; the manifest (R8) refuses to assume it (A8 §9.3) |
| **A7 F-1** — `DiffUplink.tick` documented as universe tick; production writer stamps a client-local sequence from 0; the bulk journal cannot be tick-aligned with claim windows | Re-verified today (`feed.rs:81-92`) | **Owner-first** (OD-21) | PR-7 lands the chosen half; no design in this plan assumes tick-addressed journals |
| **A7 X-C** — quantize-before-hash unpinned: swapping the two lines at `executor.rs:126-127` survives all 21 suites (every in-tree state is already lattice-integer; the snap is a live no-op) | Reproduced by A10 R-2 | **Prerequisite** for trusting the parity harness's witness leg | **Closed by the first PR** (PR-1 / F-3, §7) |
| **A8 X-1** — `RulesetId.digest` is a placeholder constant (`[0x63; 32]` Regolith, `[0x5C; 32]` Skirmish; the Skirmish comment says nothing computes one); two builds could ship identical ids | Re-verified today (`regolith/mod.rs:73-77`, `skirmish/mod.rs:94-104`) | **Owner-first** on mechanism (OD-22); prerequisite for the manifest's identity claims | R8 carries the obligation; the differential harness treats digest equality as carrying no information until the mechanism lands (A10 §4.3) |
| **A6 M-A6-4a** — the witness coverage denominator's re-delivery immunity is documented (`witness.rs:117-127`) and pinned by no check | Reproduced by A10 R-5 | **Independent**, P4-blocked (`orrery_witness` is a digest crate) | PR-10 (F-8), first post-window batch |
| **A9 D-1** — hand-off wording called the lightyear source "vendored"; it resolves from the registry (`vendor/` holds aeronet_iroh, aeronet_tokio_runtime, bevy_replicon only) | Wording drift only | **Independent** (no repo change needed; recorded so it stops propagating) | Noted here; nothing to schedule |
| **A10 N-1** — A7's "the scenario harness already holds every `TickOutcome`" is imprecise: the loop sees each outcome but retains only `{entity, inputs, hash, state}` + an event *count*; the outcome-chain fold must happen in-loop | Verified by A10 against `scenario.rs:119-129`, `:231-245` | **Independent** spec precision, already folded into F-2's spec | PR-9 implements the in-loop fold |
| **A10 N-3** — the empty suite A9 observed under M3 is the doc-test target, not a hollow integration binary | Closed by A10 | Closed | — |
| **A6 findings 1–2** — delivery-target routing and `OrderedInputs` log-order fidelity are golden-pinned, not unit-pinned | Recorded in A6 §10 | **Independent**, cheap unit tests | Folded into PR-9's scope (games crate, same window) |

---

## 5. The phased migration plan

### 5.1 The two sequencing constraints, verified

- **The P4 pipeline digest.** `PIPELINE_TREES=(crates/orrery_witness
  crates/orrery_core crates/orrery_games gates/p1-swarm)` —
  `scripts/p4-ledger.sh:409-414`, read today (the descriptive comment at
  `:33-35` names the same four; predecessors cited the comment, the array is
  the mechanism). Touching any of the four resets banked hours while the
  #329 shakedown window runs. This is temporal, not architectural: it
  orders tranches, it forbids nothing. **Outside the digest** and therefore
  window-safe: `orrery_conformance`, `orrery_persist_client`,
  `orrery_predict`, `orrery_persistd`, `orrery_protocol`, `scripts/`, new
  `gates/*` workspaces, `docs/` (A10 N-2, re-verified).
- **The ADR gate.** Implementation issues are created only after the owner
  accepts the relevant records (#395). Tranche 0 needs no acceptance;
  tranche 1 needs only the programme acceptance (fixtures pin *existing*
  behaviour); tranches 2+ need the named records.

### 5.2 The tranches

**Tranche 0 — corrections; no ADR gate; land any time.**

| PR | Scope | Non-goals | Depends on | Acceptance |
|---|---|---|---|---|
| PR-0 | DA-1 (docs/10 census: field_host, aeronet_iroh, thirteen→fifteen, add `orrery_conformance` row), DA-2 (D21 footnote), `persist.rs:41-44` tense fix (block grants are designed, unbuilt) | No behaviour, no ADR text, no F-1 disposition (that is OD-21's) | nothing | Doc-only diff; closes #414; every corrected claim carries a `path:line` cite |

**Tranche 1 — fixture hardening; window-safe; needs the programme
acceptance only.** All homes outside the digest; every fixture ships with
its kill demonstrated (the A10 rule: a fixture that has not died is not
coverage).

| PR | Scope | Non-goals | Depends on | Acceptance |
|---|---|---|---|---|
| **PR-1** | **F-3 off-lattice quantize pin** — full spec in §7 | see §7 | programme acceptance | see §7 |
| PR-2 | `projection-order-permuted` + `swarm-large` (256-entity) corpus cases in `orrery_conformance` | No naive-fold assertions (luck must not be enshrined); no executor change | PR-1 pattern | Permuted spawn order yields chain equal to forward twin; matrix runtime measured — if `swarm-large` is too slow for per-commit it moves to nightly (A10 §13.5, decided not discovered) |
| PR-3 | F-7: #417 closure fixture (`an_entity_without_the_local_marker_never_feeds_the_uplink` — an entity with `Authority`+`AuthorityPhase::LocalGranted` and no marker, so the marker clause alone refuses it), handoff-adjacent-to-rollback scenario, creation/destruction-in-window scenario (`orrery_persist_client`, `orrery_predict`) | No `feed_uplink` behaviour change; OD-31's reachability call documented in the PR either way | programme acceptance | Deleting the `With<LocallyAuthoritative>` filter kills the new test by name (today: 95 green, rustc warns `unused variable: authorities`); handoff fixture asserts ring re-anchor + exactly-one-feeder |
| PR-4 | F-6 migration round-trip goldens: committed old-format bag bytes → migrate → re-encode → compare; per-slot future-version refusal; module-removal refusal fixture (`orrery_persistd`) | No new migration steps; no quarantine tool (OD-29) | programme acceptance | `v{N}_bytes_migrate_reencode_and_match_the_committed_golden` green; committed *input* bytes, never encode-decode-encode self-checks |
| PR-5 | F-12: new `gates/migration-bench` workspace (role `check` in `scripts/check.sh`'s `WORKSPACES` table — the `p2-journal-bench` precedent) + first baseline capture `docs/plans/baselines/a10-baseline-<date>.json` (B-1..B-7 minus the F-2 leg) | No thresholds asserted in CI (doctrine: benches observe); no digest crate touched by the *code* (the baseline *run* reads them, which resets nothing) | programme acceptance | Workspace compiles in CI; baseline JSON committed with environment manifest; the differential harness's no-baseline-no-run refusal is testable against it |
| PR-6 | R2's Tier V discovery clause in `scripts/core-gates.sh`: role-keyed membership (impl/trait scan, cfg(test)-stripped, qualified paths), two-way cross-check against the declared list | No Tier H clauses (conditional, unarmed); no crate-list removal — declared ∪ discovered | **R2 accepted** | On this tree discovery reproduces exactly `{orrery_core, orrery_games, orrery_conformance}` (A4 E-D1); a synthetic impl-bearing crate fails the gate (E-D2 both directions re-run in CI self-test) |
| PR-7 | OD-21's chosen half: either `feed_uplink` stamps real ticks or `gateway.rs:377-378`/`persist.rs:200-205` docs rename the semantic | The other half | **OD-21 decided** | If code: a test pinning `DiffUplink.tick == simulation tick`; if docs: cites match behaviour |
| PR-8 | OD-26's chosen G-1 mechanism (+ F-9 trybuild compile-refusal suite with committed `.stderr`, plus the positive twin) | No byte scanning; no vendored-replicon fork unless the spike (below) forces it | **R4 accepted + OD-26 decided**; a short spike first — A9 §9 could not confirm replicon's registration API admits the extra bound without forking | Re-applying M3's payload append fails to compile at the registration site; deleting the bound flips the trybuild fixture to a named CI failure |

**Tranche 2 — first post-window batch (digest crates; after #329 / P4
exit).**

| PR | Scope | Non-goals | Depends on | Acceptance |
|---|---|---|---|---|
| PR-9 | F-2 outcome-chain goldens in `orrery_games`: in-loop fold (A10 N-1) of events ‖ materialized ids ‖ delivery pairs, WP-2-ordered; committed `REGOLITH_OUTCOMES`/`SKIRMISH_OUTCOMES`; plus the two cheap unit pins from A6 findings 1–2 (delivery-target, log-order fidelity) | No wire change, no evidence change — chains are fixtures (A7 §6's boundary); no golden regeneration of state chains | P4 exit; programme acceptance | Re-applying A7 X-A kills `outcome_chains_match_the_committed_golden` on tick 1 of every scenario while state chains stay green; a `deliver`-arm flip to `None` moves the chain on the emission tick |
| PR-10 | F-8 witness re-delivery coverage pin in `orrery_witness` (three-legged: deliver, re-deliver unchanged, extend advances exactly) | No fold change | P4 exit | M-A6-4a's mutation kills leg 2; the inverse (advance=0) kills leg 3 |

**Ordering rule (hard): PR-9 merges before any tranche-3 composition PR.**
Outcome goldens generated after composition changes would commit the
candidate's behaviour as "legacy" (A10 §8.3). P4 exit precedes Phase 2 in
every plan variant, so the rule costs nothing.

**Tranche 3 — Phase 2: composition behind the existing contract.** Needs
**R1** (and R8 for the manifest struct's shape); touches `orrery_games` +
one new crate; post-window.

| PR | Scope | Non-goals | Depends on | Acceptance |
|---|---|---|---|---|
| PR-11 | Composition-root skeleton: the plain struct-of-tables manifest (A8 §3.1), composition-time validation (duplicate ids, missing/cyclic deps — the F-10 battery's refusing spine), X-5 registry file for Regolith | No behaviour change; no ECS; no dynamic anything (D21) | R1, R8; PR-9 | State **and** outcome chains byte-identical; validation battery kills each refusal by construction (F-10 named tests); `core-gates.sh` green |
| PR-12 | First two Regolith domains split into delegated modules behind the one assembled `Ruleset` (phase-2 exit: "at least two existing behaviours owned by separate modules") | No trait change; no schedule; no storage change | PR-11 | All goldens + outcome chains unchanged; the module boundary visible in the manifest tables; A2 §5.3's four properties (visible, ordered, owned, event-composed) demonstrable per coupling |
| PR-13 | X-2 manifest keyspace family in `orrery_persistd` (additive door of D21) + schedule-digest/`projection_version` fields carried (values: current topology, 1) | No handshake change (OD-24 open) | R8; PR-11 | Registering a build writes its manifest once; retained permanently; readable without linking the game |
| PR-14 | X-1 digest computation per OD-22's mechanism; placeholders replaced in both games | No verification-at-admission (a later, separate door) | OD-22; R8 | Two builds differing in any determinism-relevant source produce different digests; the id round-trips through claims/bundles unchanged in shape |

**Tranche 4 — Phase 3: the `SimulationHost` seam.** Needs **R1**;
post-window (touches `orrery_core` adjacency and, when hosts converge,
`gates/p1-swarm`).

| PR | Scope | Non-goals | Depends on | Acceptance |
|---|---|---|---|---|
| PR-15 | Host crate: tick advance, stable-id lookup (N-2's single index), command-in/event-out, output collection; existing `Ruleset` hosted through an adapter; headless tests drive the same API the client will | No state moves; no ECS; storage stays the executor behind the seam | R1; PR-11 | Bevy client test-double and headless tests invoke one API; host lifetime and fixed-step semantics explicit (phase-3 exit criteria verbatim); goldens unchanged |
| PR-16 | Converge the three tick drivers: regolith client, then p1-swarm bots, onto the host; promote the bots' harness-side frame/claim assembly (`bot.rs:1103-1137`) into the host's output path | No witness pipeline change; one driver per PR, revertable separately | PR-15; P4 exit (bots are digest territory) | p1-swarm gate criteria unchanged; per-driver diff shows deletion of a hand-rolled loop, not a behaviour edit |

**Tranche 5 — conditional; scheduled only when its trigger fires; each
item's precondition is pre-registered.**

| Item | Trigger / condition | What it is |
|---|---|---|
| Tier H gate battery + E-M2 canary + E-M3 projection differential + worker/profile matrix legs | An R1 trigger (T1–T3) fires **and** OD-23 decided (the profile leg must land with the overflow policy) | A4 §5.2/§6; A10 F-5 |
| ECS pilot vertical slice (brief Phase 4) | Tier H landed and demonstrated ≥ today's clauses; A5 registry live; G-3 four-class differential harness live; capacity-scale mirror numbers replacing P4/P-1's indicative bounds | A3 §7.4's precondition list, unchanged |
| Per-module domain migration (brief Phase 5) | Pilot promoted on its pre-registered criteria | A10 §4's per-module differential recipe; legacy path removed only after four-class parity |
| Presentation-frame schema + AOI extraction + F-11 (brief Phase 6) | First consumer (Unreal sidecar or a client refactor) — nothing exists to fixture today (A10 V16) | A9 §2.4's contract |
| Generic/compat cleanup incl. `classify_component` removal (brief Phase 7) | Registry restates the three impls' facts first (A5's sequencing rider: the method goes last) | R4's tail |
| Unreal observer proof (brief Phase 8) | Owner supplies the Unreal requirement (OD closes decision 17); host seam exists | A9 §5's three components and four falsifiable checks (P-1..P-4) |

### 5.3 Compatibility adapters

The no-flag-day machinery, named per seam:

1. **The assembled ruleset is the adapter.** Under PR-11/12 the game still
   presents one `Ruleset` to the executor, witness, persistd and goldens —
   composition is invisible below the trait. No consumer changes.
2. **The host adapter.** PR-15 hosts the *existing* `Ruleset` through an
   adapter (brief phase 3's own design); hosts migrate one at a time
   (PR-16), each revertable alone.
3. **`classify_component` stays until the registry restates its facts** —
   at no point does the tree hold less classification information than
   today (A5 §6.1's rider).
4. **Dual-chain pinning is the parity instrument.** State chains (exist) +
   outcome chains (PR-9) bracket every subsequent move; the four-class
   differential harness (F-4) extends it across implementations when a
   candidate exists.
5. **No wire change anywhere in tranches 0–4.** `protocol_accepted` stays
   exact equality (mutation-pinned, A8 M-A8-1); the manifest lives in the
   keyspace, not the handshake, until OD-24 says otherwise.

### 5.4 Rollback strategy for the migration itself

- **Every PR is additive and git-revertable without residue.** No PR in
  tranches 0–4 changes wire bytes, persisted formats, or golden values
  (PR-14 changes digest *values* inside an unchanged shape; its revert
  restores the placeholders — safe because nothing verifies digests until
  the later admission door). A revert therefore needs no data migration
  and no protocol step.
- **Behaviour is pinned before it moves.** The F-2-before-Phase-2 rule
  means any tranche-3+ revert is *verifiable*: chains must return to their
  committed values, and a revert that doesn't is itself a finding.
- **Static composition is the feature flag.** There are no runtime flags
  to strand: a module not assembled is a module not present, and the
  manifest diff in the PR is the flag flip — reviewable, bisectable,
  revertable.
- **Digest-aware reverts.** Tranche-1 reverts touch no digest crate and
  cost no banked hours; tranche-2+ reverts happen post-window where the
  digest no longer taxes them.
- **Phase exits are checkpoints with named fallbacks.** If the seam is
  judged speculative structure, V1 (composition only) is the recorded
  fallback and nothing else changes (second opinion §6); if composition
  fails at scale, the pivot is H1, pre-registered (§1); if Tier H cannot
  demonstrate strength ≥ today's clauses, the pilot stays closed
  regardless of other merits (A3 §7's own standard).
- **The X-2 manifest family is append-only**, so no rollback orphans rows:
  a reverted build's manifest record simply stops being referenced.

---

## 6. The brief's phases 0–8, mapped

The brief calls its sequence "intentionally provisional" and asks for it to
be reordered or rejected on evidence. Verdict per phase:

| Phase (brief) | Verdict | Where it lands |
|---|---|---|
| 0 — archaeology and dependency map | **Done**, exceeded | A1 (map + ten assumptions verdicts), A2 (ownership), A3 §1 (ground truth). Outstanding sliver: the build-time/binary-size baseline the brief lists — B-7, captured by PR-5 |
| 1 — behavioural conformance fixtures | **Programmed** | A10's fixture set; tranches 1–2 (PR-1..PR-5, PR-9, PR-10). The brief's own exit criterion ("existing behaviour comparable mechanically with a replacement") is *not yet met by goldens alone* — X-A proved it — which is why F-2 is mandatory before Phase 2 |
| 2 — composition without ECS | **Scheduled** | Tranche 3 (PR-11, PR-12), gated on R1/R8 acceptance and PR-9. Purpose kept verbatim: separate the module-model decision from the ECS-storage decision |
| 3 — canonical simulation host | **Scheduled** | Tranche 4 (PR-15, PR-16). Reordered *after* Phase 2 lands its skeleton, matching both A3 lanes; the brief's own exit criteria adopted unchanged |
| 4 — `bevy_ecs` behind the host | **Conditional — not scheduled** | Trigger-gated (T1–T3) with the pre-registered precondition package (tranche 5). The digest forbids nothing here; the *evidence* does: every immediate benefit currently lacks a consumer (A3/second opinion E-7) |
| 5 — incremental domain migration | **Conditional** | Follows a promoted pilot; A10 §4's per-module differential recipe is the mechanism; the brief's seven-step per-module list survives intact |
| 6 — canonical/presentation isolation | **Largely already true; remainder gated** | The isolation the phase wants *ships* (dedicated store, mirrors, A9 §2). The un-built remainder — presentation-frame schema, AOI extraction contract, F-11 — waits for its first consumer (A10 V16: nothing exists to fixture) |
| 7 — generic and compatibility cleanup | **Tail, partially reordered** | `classify_component` removal sequenced last (A5 rider); `R:` propagation is already narrow (A1 §4.4) so the phase's "reduce workspace-wide propagation" has little to do; adapter removal follows parity per module |
| 8 — Unreal-facing proof | **Deferred, specified** | A9 §5's observer proof with falsifiable checks P-1..P-4; opens when the owner supplies the Unreal requirement (decision 17) and the host seam exists |

**No flag-day migration** holds by construction: at every tranche boundary
the tree is releasable, all gates green, no dual implementation past its
parity proof, and the wire untouched.

---

## 7. The exact first PR

**PR-1 — pin quantize-before-hash (fixture F-3).** The first
implementation PR of the programme (PR-0, the docs census fix, can precede
it independently and needs no acceptance at all).

**Why this one first:** it closes a live, twice-reproduced coverage hole
(A7 X-C; A10 R-2: hashing before quantizing survives all 21 suites today);
it is the smallest possible change — one test-only ruleset, one named
test; it touches only `orrery_conformance` (outside the P4 digest, inside
the root workspace, so it runs in existing CI with no `check.sh` table
edit); it changes zero behaviour; and every later parity argument leans on
the property it pins ("a claim commits to exactly what replication and
persistence saw", `ruleset.rs:319-326`).

**Scope (files):**

- `crates/orrery_conformance/tests/quantize_pin.rs` (new): a minimal
  test-only `Ruleset` whose `CoreState` holds a continuous field in raw
  micrometres and whose `step` writes a deliberately off-lattice value
  (e.g. `pos += 1_499` µm against the 1 mm lattice); `quantize()` snaps
  half-away-from-zero per `quantize.rs`. One named test:
  `the_claimed_hash_is_of_the_quantized_state_not_the_raw_one` — drive one
  entity one tick through `Executor::step_entity`; compute
  `state_hash(quantized_expected)` and `state_hash(raw_expected)`
  independently in the test; assert the outcome's hash **equals the
  former and differs from the latter**.
- Nothing else. No source file of any other crate.

**Acceptance criteria (each testable):**

1. The named test passes on an unmodified tree.
2. **Mutation kill demonstrated in the PR description:** re-applying X-C's
   two-line swap at `executor.rs:126-127` makes the named test fail;
   reverting makes it pass. (This converts X-C from "survived everything"
   to "killed by one named check".)
3. **Vacuity self-check:** the in-test assertion `hash(quantized) ≠
   hash(raw)` means an accidentally on-lattice fixture fails itself rather
   than silently pinning nothing — the #417 lesson applied to the
   fixture's own construction.
4. `./scripts/core-gates.sh` exits 0 (the test ruleset obeys VC-4/6/8 —
   `orrery_conformance` is a gated crate, which is deliberate: the fixture
   lives under the same discipline as the code it pins).
5. The P4 pipeline digest is byte-identical before and after
   (`p4-ledger.sh`'s four trees untouched) — stated and checked, so the
   PR is provably window-safe.

**Non-goals (explicit):** no change to `Executor` or `quantize.rs`; no new
corpus case (PR-2's job); no golden regeneration; no fix of any other
finding; no ADR text.

**Dependencies:** the programme acceptance (§2's last block). Nothing
else — no R-record is needed to pin behaviour that docs/06 VC-7 already
states and the executor already implements.

---

## 8. Traceability

### 8.1 The #395 table, filled

| Brief section | Lands in | Outcome |
|---|---|---|
| Assignment (10 points) | A1–A11 | 1→A1 (surface + call sites); 2→A1 §4.4 + A3 disputed-claims ledgers (real-vs-speculative separated claim by claim); 3→A3 (five variants, two independent lanes); 4→A3 §7 / second opinion §6 (recommendation with what it beat); 5→A11 §5 (phased PRs); 6→A11 §5.3–5.4 + §6 (gates preserved, no flag-day); 7→A11 §3 (twenty + twelve surfaced); 8→A11 §2 (eight records, three amendments); 9→A10 (fixtures/benchmarks before implementation); 10→conflicts called out: V2 vs the strike economy (A3), the gate's coverage-vs-name gap (A4 §1.2), rolling upgrade vs D29 (A8 §6), event-sourcing vs state-commitment doctrine (A7 §4.5) |
| Current-state assumptions (10) | A1 §9 | 8 confirmed, 1 corrected (#5: lightyear coupling is configuration-layer; rules-execution Bevy coupling lives in witness plugin + clients), 1 unverifiable (#10: no Unreal evidence exists in-tree; treated as an imported requirement) |
| Architecture variants (4) | A3 + second opinion | Five scored (brief's four + a tree-suggested hybrid), two independent weighted matrices; shared world rejected in both by unbridgeable margins; convergent action adopted (§1); V3 preserved as the trigger-gated future, V4's substance kept without its foreclosure |
| Decision matrix | A3 §5–§6; second opinion §4–§5 | Weights justified before scoring in both lanes; sensitivity passes recorded (V1's lead survives hostile reweighting; V5-over-V1 named as inside method noise with V1 the fallback); unevidenced claims excluded from arithmetic by rule |
| Phases 0–8 | A11 §6 | 0 done · 1 programmed · 2–3 scheduled · 4–5 conditional (trigger-gated) · 6 partially shipped, remainder consumer-gated · 7 tail · 8 deferred-specified |
| Owner decisions (20) | A11 §3.1 | All twenty dispositioned: 9 answered, 2 settled by accepted records, 7 proposed (each naming its record), 2 deferred (each naming its condition), 0 withdrawn — plus 12 surfaced decisions (OD-21..OD-32) in §3.2 |
| Expected deliverable (9 sections) | A1–A11 | 1 Repository evidence→A1(+A2 §3); 2 Problem validation→A1 §4.4/A3 §8/second opinion §7; 3 Variants→A3 both lanes; 4 Recommendation→A3 §7 (component model: per-entity executor + capability registry; scheduling: A4 S0–S7; composition: A8 §3.1 struct-of-tables; persistence/rollback/witness: A7 P-1/R-1/WP-1..6; boundaries: A9); 5 Migration plan→A11 §5; 6 Verification plan→A10; 7 ADR plan→A11 §2; 8 Open decisions→A11 §3; 9 First PR→A11 §7 |

### 8.2 Every remaining brief section, mapped

The epic's table rows cover the load-bearing sections; the brief has
further headings, and "every section maps to a decision, a task, or an
explicit deferral" means all of them:

| Brief section (line) | Maps to |
|---|---|
| Executive summary / working hypothesis (:42-59) | Critiqued and partially adopted: composition root yes, ECS substrate no-for-now (R1). The hypothesis's own caveat ("ECS does not deliver determinism…") is what A4–A8 built above the storage question |
| Motivation: what is good / what becomes dangerous (:63-105) | Validated as narrow-real + scale-speculative (A1 §4.4, A3 C1); the god-trait failure list is the pre-registered E-1 experiment's checklist, not a present fact |
| Proposed conceptual model: kernel (:130-146) | A2 §2 rows 1–8 (ownership decided); the "may use bevy_ecs internally" clause is R1's trigger-gated door |
| Rules modules (:148-175) | A2 (ownership) + A8 (module table, versioning) + tranche 3 (tasks). Illustrative names not adopted (brief's own instruction) |
| Integration modules (:177-195) | Evaluated both ways in A2 §5; construct deferred to the composition root's evidence (A2 §5.3's four properties are the binding requirement); decision explicitly *not* forced |
| Game definition / assembled ruleset (:197-231) | A8 §3.1 decided the construct (struct-of-tables, not the illustrative trait); PR-11 |
| Dedicated canonical world + costs/benefits (:236-272) | Decided (R1): dedicated permanently — it already ships; costs measured (mirror µs-scale), benefits inventoried; single-world comparison done twice |
| Stable identity (:274-294) | R3 (all seven bullet questions answered in A5 §2: creation/allocation, lookup ownership, despawn/tombstones, rollback of maps, cross-island/cell, staleness, predicted identity class) |
| Deterministic scheduling model + 10 questions (:297-341) | R2; A4 §3 answers all ten (envelope; parallelism = S2 across entities; event ordering = producer-total-order; deferred commands = S6 only; structural order = emission/FWW; query order never observable; float policy = VC-5/6/7; RNG partition = VC-3; topology hash = schedule digest; violation detection = E-M1..13) |
| Component policy registry (:346-380) | R4 (five dimensions, not one object — the brief's own overgeneralization warning honoured); reflection clause adopted verbatim as N-6 |
| Commands, events, and queries (:384-407) | R5; every listed item (ordering, delivery, replay, rollback, dedup, volume, stored-vs-derived, cyclic subscription) has a rule row in A6 §3 |
| Persistence / rollback / witnessing (:410-449) | P-1 (no new strategy — the tier hybrid stands), R6 (unit), R7 (projection; the manifest-identifies list is A7 §7.2); strategy comparison in A7 §4 covers all five candidates incl. the brief's world-clone skepticism, answered structurally |
| Bevy integration variants A/B (:453-470) | Decided: A (dedicated) permanently, B rejected; SubApp vs manual `World` answered — manual, `SubApp` drags `bevy_app` (second opinion E-9/C-8) |
| Unreal integration implications (:474-511) | A9 §4 [SPEC] + §5 observer proof; boundary rules adopted; deferred behind decision 17 |
| Variants to compare + matrix criteria (:515-577) | A3 both lanes; every suggested criterion appears in one or both weight tables |
| Compatibility and versioning (:581-608) | R8; all eight "plan must determine" bullets answered in A8 §§4-8; static recommendation ratified |
| Migration principles (:612-623) | All ten adopted; #9 (compatibility adapter) is §5.3; #10 is the tranche structure itself |
| Candidate phased migration (:627-771) | §6's per-phase verdicts (reordered/gated exactly where evidence demanded) |
| Testing strategy (:775-829) | A10 wholesale: determinism §5, differential §4, persistence §6.1, rollback/prediction §6.2, modularity §7.1, performance §8 |
| Primary risks (:832-895) | Each risk has an owner: Bevy churn→R8 pins + D14 + narrow facade (and no bevy_ecs adoption yet); false determinism confidence→refuse-over-observe doctrine; Lightyear duplication→R6/L-1 ownership split; two-world overhead→measured, bounded, AOI-scoped; overgeneralized module API→five registries not one object, struct-of-tables; premature dynamic→D21 ratified; protocol leakage→IV-7/WP-5/G-1 closure (OD-26) |
| Suggested agent prompt (:975-979) | Consumed — it is the epic's own working method; no decision content |
| Initial recommendation (:983-1000) | Points 1–2, 5–10 adopted; points 3–4 (ECS substrate, dedicated world *now*) amended to trigger-gated (R1) — the one place the plan diverges from the brief's initial preference, with two independent matrices as the reason |
| Reference material (:1004-1010) | Checked where load-bearing (docs/10 census wrong three ways → DA-1; ADR-0004 exists; Bevy 0.19 pinned) |

---

## 9. Stale citations found while verifying

| Record | Citation / phrasing | Current truth |
|---|---|---|
| A7 §9.5, A10 V10 (and this node's own briefing) | "`p4-ledger.sh:33-35` hashes core/games/witness/p1-swarm" vs the briefing's "`scripts/p4-ledger.sh:409-413`" | Both point at real text, neither precisely: `:33-35` is the *comment* naming the four trees; the mechanism is `readonly PIPELINE_TREES=(…)` at **`:409-414`** (the briefing's `:413` stops one line short of the closing paren). Contents verified identical either way; findings unaffected |
| This node's briefing | "A10's N-2 … `PIPELINE_TREES` is `crates/orrery_witness`, `crates/orrery_core`, `crates/orrery_games`, `gates/p1-swarm`" | Verified verbatim today (`p4-ledger.sh:409-414`); the window-safe set (`orrery_conformance`, `orrery_persist_client`, `orrery_predict`) confirmed outside it |
| A8 I5 / this node's briefing | Placeholder digests at `regolith/mod.rs:74-77` / `:76` and `skirmish/mod.rs:94-104` / `:102` | Re-read today: Regolith `[0x63; 32]` at `:73-76`, Skirmish `[0x5C; 32]` at `:100-103` with the nothing-computes-one comment at `:94-99` — ±1–2-line drift, claims exact |
| A9 D-2/D-3, #414 | docs/10-crates.md census | Re-verified all three ways today: "thirteen" at `:3`, `orrery_aeronet_iroh` under `crates/` at `:18`, `orrery_field_host` at `:29` and in layering rule 2 at `:95`; `crates/` holds fifteen `orrery_*` members including `orrery_conformance` |
| Inherited stale set (A1–A10 records): ADR-0038 `ruleset.rs:211` drift; D21 `validate_intent` parenthetical; docs/06 `:60`/`:210` present-tense `classify_component` consumers; `persist.rs:41-44` block-grant tense; A7 "DiffUplick" typo; A9 M3's non-compiling literal description; bot.rs producer-line drift | — | Not re-litigated; where this document leans on the same ground (D21 freeze text `:61-64`, `:85-90`; D38 (c) `:161-169`, (d)(3) `:198-205`; `feed.rs:62-92`; `golden.rs:22-28`; `gateway.rs:164-184`; `battery.rs:222`) the lines were re-opened today and held |

No citation this document relies on from AGENTS.md proved wrong during
this task.

---

## 10. Verification and mutation log

**Tree identity (the re-basing fact):** this branch is docs-only over
`main` at `2b542c4d` — `git diff --stat 2b542c4d HEAD -- crates gates
scripts vendor` is empty. Every predecessor mutation therefore ran against
byte-identical code, and their logs (twelve documents' worth, §"Method")
carry at full strength; A10's log additionally ran against this same base
commit.

**First-hand verifications (steady-state, no mutation needed):**
`PIPELINE_TREES` (`p4-ledger.sh:409-414`); `GATED_CRATES`
(`core-gates.sh:37`) and `RULES_CRATES` (`:42`); placeholder digests (both
games, with Skirmish's admission comment); `feed_uplink`'s guard order and
seq-as-tick stamping (`feed.rs:62-92`); the goldens blind-spot doc
(`golden.rs:22-28`); `protocol_accepted` exact equality with the D29
clause-5 closure comment (`gateway.rs:164-184`); `game_test!` generating
`chains_match_the_committed_golden` (`battery.rs:222` — a grep for
`fn chains_match` finds nothing, per the macro-name rule); docs/10 census
(all three errors); D21/D38 quoted clauses; fifteen `orrery_*` crates on
disk; issues #414 and #417 OPEN, #418 (A4) MERGED, PR #423 (A10) OPEN.

**Mutation (break stage → named check dies → revert → passes):**

| # | Guarded stage broken | Named check | Observed | Reverted |
|---|---|---|---|---|
| M-A11-1 | `[dev-dependencies] bevy_ecs = "0.19"` appended to `crates/orrery_conformance/Cargo.toml` — an engine entering a gated crate's graph through the weakest insertion point, on the exact crate PR-1 targets | `./scripts/core-gates.sh` clause 1 | Baseline first: all four clause notes print, `verifiable-core static gates pass`, exit 0. Mutated: `core-gates: orrery_conformance has Bevy in its dependency graph`, exit **1** | `git checkout` of the manifest; gate exit **0**; `git status` clean |

One mutation, chosen deliberately: it proves live, on this exact tree, the
one enforcement claim this document adds weight to (tranche 1 keeps every
gate green, and PR-1's home crate is watched by the clause that matters).
Everything else this document asserts as enforced is asserted on a
predecessor's mutation over byte-identical code, cited by its log entry
rather than re-run — re-running all thirty-plus would add heat, not light.

---

## 11. Unsure

Stated as unsure rather than smoothed over:

1. **A10 is consumed from an open PR.** #423 is unmerged; this document
   cites the branch head. If review changes A10's fixture formats or
   sequencing (particularly N-2's window-safe set or the F-2 fold), §5's
   tranche 1–2 contents follow A10's merged text, not this snapshot.
2. **The eight-record partition is a judgement call.** The substance is
   the nodes'; the grouping is mine. An owner who prefers fewer, larger
   records (e.g. R6+R7 as one A7 record, or R3+R4 as one identity record)
   loses nothing but review granularity; the map in §2 makes re-cutting
   cheap. The one partition I would defend hard is keeping R7 separate:
   it is the only proposal that amends an accepted record's normative
   text (D38's axis law).
3. **PR-16's promotion of harness-side frame assembly into the host** is
   the least-specified PR in the plan: A3's second opinion (§11.3) already
   doubted how much of the seam can be extracted without deep p1-swarm
   surgery, and no node prototyped it. Its scope may split further at
   implementation-issue time; the acceptance criterion (gate criteria
   unchanged, hand-rolled loop deleted not edited) is the stable part.
4. **Whether tranche 1 truly needs no ADR** rests on my reading that
   fixtures pinning existing, documented behaviour are programme work, not
   normative change. PR-6 (the discovery clause) is the borderline case —
   I gated it on R2 acceptance precisely because it edits a gate, even
   though it removes nothing.
5. **Decision 17's condition ("owner supplies the Unreal requirement") has
   no owner-visible artifact named.** I could not find a template or issue
   for it; if the owner wants Phase 8 to open, the first step is a short
   requirement note (embedded vs sidecar first, latency budget, prediction
   scope) that A9 §9 already itemized.
6. **Counts.** "Twelve documents' worth" of mutation logs and the
   disposition tally (9/2/7/2) were counted by hand from the sources cited;
   a recount is cheap and the tables are the authority if I miscounted.

Deliberately not done:

- **No ADR text was drafted.** §2 specifies each record's scope, sources
  and amendments; drafting the records is the natural next task *after*
  the owner indicates which to take up, and drafting all eight before that
  signal would front-load text the owner may re-cut (§11.2).
- **No implementation, no issue creation.** The PR plan is a plan;
  implementation issues follow acceptance, per the epic's header.
- **No decision was made that any node or the owner reserved.** Every
  reserved item appears in §3 with its owner named.
