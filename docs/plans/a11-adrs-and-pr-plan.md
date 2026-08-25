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
