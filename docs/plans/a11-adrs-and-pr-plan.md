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
