# Plan-staleness sweep — the S7 sanction and the `orrery_games` ECS acceptance, applied below the ADR layer

**Documentation-only lane.** Two owner decisions changed the facts and a
number of plan and measurement documents still described the world before
them. This document records what was swept, what was corrected, and what was
left for the owner. Branch: `docs/plan-staleness-sweep`. Nothing here amends
an Accepted ADR — amending one is owner-reserved
([AGENTS.md](../../AGENTS.md)); `check.sh` is exempt (prose; AGENTS.md, "The
push is the gate"); `./scripts/lane-diff-audit.sh` passes on this diff.

The two decisions:

1. **2026-08-30** — the owner sanctioned S7 entry directly; no D42 (d)
   trigger fired ([#745](https://github.com/baadc0de/orrery/issues/745),
   owner sanction in the issue body).
2. **2026-08-31** — the owner accepted `bevy_ecs` as a first-class
   dependency of `orrery_games`, with the ECS as idiomatic storage for
   entities and ruleset logic and systems registration and tick driving over
   `World` ([#793](https://github.com/baadc0de/orrery/issues/793), comment
   "OWNER ACCEPTANCE, 2026-08-31").

The three landings this sweep tested claims against:

- Tier H landed and is enforced mutation-style — battery at
  `scripts/core-gates.sh` section 6 (#762), recorded as paid and the
  confinement lifted in D43 clause (e) (#771),
  `DECLARED_HOST_CRATES=(orrery_sim_host)`.
- The F-4 differential harness landed with all four A10 §4.1 classes —
  `crates/orrery_games/src/diff.rs` (#748 D-1/D-2, #749 D-3/D-4;
  generalized across substrates at #757).
- The host landed behind the seam — `orrery_sim_host`'s `EcsBackend` (#757),
  at four-class F-4 parity.

Each fact was verified on this tree before any correction was written:
`diff.rs`'s own header names the four classes; `ecs.rs` defines `EcsBackend`;
`core-gates.sh:259` carries `BEVY_PERMITTED_CRATES=(orrery_games)`, `:510`
carries `DECLARED_HOST_CRATES=(orrery_sim_host)`; `crates/orrery_games/
Cargo.toml` declares no Bevy (the acceptance is recorded, the dependency not
yet taken); D43 clause (e)'s two 2026-08-31 amendments carry the discharge.

#853 ([d42-d-amendment-draft.md](d42-d-amendment-draft.md)) audited what
depends on D42 (d) specifically and named A18 §5 and A22 as the plan-doc
follow-ups it deliberately left. This sweep searched **independently** of
that list — for "no trigger has fired", "Tier H is empty", "bevy_ecs is not
a dependency", harness-does-not-exist, and anything asserting the ECS is
unadopted — and found hits #853 did not name: A18 §5's S2 scope note, A4's
Tier H section, A9's owner-artifacts bullet, and A10's F-4 fixture row.

---

## 1. Corrected (subordinate plan documents; 5 files, 15 claims)

Every correction states the fact, cites its record, and preserves the
document's argument and dating. Where a document has its own landed-note
convention (A22's "Landed, #744" pattern), the correction follows it.

| File | Line (pre-edit) | What it said | What is true now |
|---|---|---|---|
| [a18-ruleset-ecs-implementation-programme.md](../plans/a18-ruleset-ecs-implementation-programme.md) | :361-362 (§5 S2 scope) | "No Tier H clauses - D43 clause (e) arms only on a D42 trigger, and none has fired." | D43 (e) arms per declared host (amended 2026-08-31); Tier H landed #771; the host was admitted by owner sanction, not a fired trigger. |
| same | :488-507 (§5 S7 block) | Heading "Conditional; nothing here is scheduled"; body "**T3 is a necessary precondition regardless of T1/T2.** None has fired; Tier H is empty by D43 clause (e)'s own accounting." | Entry was sanctioned directly (#745, 2026-08-30); Tier H landed (#771); the seam substrate landed as `EcsBackend` (#757) at four-class F-4 parity. The block now records current standing: harness live, A5 registry partial (D45's open items), capacity mirror numbers unmet and partly overtaken, `orrery_games` acceptance recorded but the dependency not yet taken. |
| same | :529 (§6 graph) | "S7 (conditional; no date)" | "S7 (entry sanctioned 2026-08-30; #757 landed)". |
| [a22-engine-agnostic-client.md](../plans/a22-engine-agnostic-client.md) | :13-14 (header) | "…nothing here authorises an ECS adoption: D42 clause (d) trigger-gates that and no trigger has fired." | The document's own non-authorisation stands; the trailing clause now names the two recorded owner decisions (#745; #793) instead of implying adoption was unavailable. |
| same | :26-28 (§1) | Clause 1 "enforces the boundary today by scanning `GATED_CRATES=(orrery_core orrery_games orrery_conformance)` for Bevy." | Boundary narrowed 2026-08-31: `orrery_games` is `BEVY_PERMITTED_CRATES`' single entry (#793); `orrery_core` and `orrery_conformance` stay Bevy-free unconditionally. |
| same | :41 (§2 table) | Gate cell: "D42 (d) trigger: T1, T2 or T3" | "Owner-sanctioned entry, 2026-08-30 (#745); no D42 (d) trigger fired." |
| same | :43-49 (§2) | "D42 (d) admits a canonical `bevy_ecs::World` only when … fires. None has." | Trigger framing kept as the automatic path; sanction and #757 landing added. The Unreal-doesn't-need-the-ECS-move argument is untouched. |
| same | :140 (§6 track E) | "only on a fired D42 (d) trigger \| last, or never" | Sanctioned directly; the substrate landed 2026-08-31 (#757), ahead of the table's own ordering. |
| same | :196 (§7 item 4) | "Any ECS adoption - D42 (d), unchanged and untouched here." | The reservation is restated with the two recorded decisions; D42 (d) is not amended by A22 (that part was true and stands). |
| [a4-deterministic-execution.md](../plans/a4-deterministic-execution.md) | :393 (§5.2 heading) | "Tier H — host machinery (exists only if A3's trigger T3 ever fires)." | Landed 2026-08-31 (`scripts/core-gates.sh` section 6, #771), armed per declared host; at plan time the conditional was as written. |
| same | :410-411 (§5.2) | "Until a trigger fires, Tier H is empty and the tree is exactly Tier V plus the unchanged witness adapter situation." | No longer vacant: landed (#771), arms per declared host (`DECLARED_HOST_CRATES`); the witness-adapter exception stands unchanged. |
| same | :447-448 (§5.4) | "Tier H lands only with, and gated behind, an actual ECS-host trigger." | Overtaken: landed with the sanction-admitted host, arming per declared host. |
| same | :603-606 (§11 item 5) | "Tier H remains entirely conditional … most of this document's *new* enforcement is untested against production pressure until/unless ECS is admitted." | Battery enforced and demonstrated mutation-style in full (D43 (e)'s amendments); the conditional-vacant posture is history. |
| [a9-engine-boundaries.md](../plans/a9-engine-boundaries.md) | :425-426 (§8) | "the Tier-H gate bundle remains conditional on A3's triggers; nothing here arms it." | Landed (#771), enforced mutation-style, arms per declared host; "nothing here armed it" stays true and is kept. |
| [a10-conformance-benchmarks.md](../plans/a10-conformance-benchmarks.md) | :300 (§3 F-4 row) | Status "proposed" | **Landed** — `crates/orrery_games/src/diff.rs`, all four §4.1 classes (#748/#749; generalized across substrates at #757), not the planned `orrery_conformance`-plus-runner home. The Home and Named-check columns are left as the plan's original layout; the status cell carries the correction. |

## 2. Owner-reserved — listed, not touched

- **[D42](../adr/0042-canonical-simulation-architecture.md) clause (d)'s
  body** (`:338-425`) still reads trigger-gated. The restatement is #853's
  draft, awaiting owner acceptance; until then the record is exactly what it
  was, and this sweep did not touch it.
- **D42's title** (`:1`, "… dedicated ECS world is trigger-gated") — #853 §5
  names it for the owner's accepting edit.
- **[DECISIONS.md](../DECISIONS.md):57** — the D42 index row still reads
  "dedicated ECS world trigger-gated". The index of Accepted records is the
  same owner call as the title (index rows carry amendment history; #853 §5
  names it).
- **[D43](../adr/0043-determinism-envelope-and-gate-replacement.md) clause
  (e)**'s heading (`:332-334`, "conditional host battery, armed only by a
  D42 trigger") and its honest-accounting paragraph (`:519-526`, "Until a
  trigger fires, Tier H is empty") carry the same body-vs-blockquote
  layering. A companion D43 restatement is #853's recommended follow-up —
  owner's call, not drafted here.
- **D44 (b)** (`:222-225`) and **D47 (b)** (`:163-169`) — forward-conditional
  wording ("a triggered dedicated ECS world"); #853 §3.1 classes the
  staleness as cosmetic with the normative content untouched. Owner-reserved;
  not touched.

## 3. Deliberately left — history, argument, or dated snapshot

The rule applied: live present-tense claims about the tree get corrected;
a document's argument, recommendation, evidence-gathering tables, dated
audit tables, and incident snapshots do not.

- **a18:63** — §1's recommended-sequencing line ("leave the ECS itself
  trigger-gated where D42 clause (d) put it (S7)"). This is the document's
  argument; the owner's sanction answered it. Rewriting the recommendation
  would rewrite the argument, which this lane does not do.
- **a22:51-54** — "T3 is a necessary precondition for any canonical byte to
  move regardless." T3's necessity is still on the books (D42 (d)'s
  amendment) and is now *satisfied* (Tier H demonstrated); the sentence sits
  inside §2's argument. Left.
- **a22's §1 quote of D42 (a)** (`:21-24`) — the quoted body text stands;
  the 2026-08-31 amendments are additive blockquotes on it. Left.
- **[a3-simulation-host-comparison.md §7](../plans/a3-simulation-host-comparison.md)**
  (`:436-504`) and the [second opinion](../plans/a3-simulation-host-second-opinion.md)
  (`:345, :455, :470`) — the provenance of T1–T3 and the precondition
  package. #745's audit comment names A3 §7 for the owner's accepting edit
  ("will read stale afterwards and should be named in the decision"); #853
  §3.2 declines to draft it because the restatement leans on this text.
  Left here for the same reason.
- **[a11-adrs-and-pr-plan.md](../plans/a11-adrs-and-pr-plan.md)**
  (`:56, :59-69, :84, :162, :252, :294-295, :343, :472`) — the planning
  record of 2026-08-25 (A18's own header: "A11 section 5's tranche table
  remains the record of what was planned on 2026-08-25"). Lines like :472
  ("no bevy_ecs adoption yet") and :294-295 (trigger-gated tranche
  conditions) were true when written. Left.
- **[a12-exchange-systems-shakedown.md :20-33](../plans/a12-exchange-systems-shakedown.md)**
  — dated incident snapshot quoting D42 as accepted at the time; incident
  records do not track later amendments (#853 §3.2 concurs). Left.
- **Dated evidence rows**: [a5:52](../plans/a5-identity-and-capabilities.md),
  [a7:54, :56](../plans/a7-persistence-rollback-witnessing.md),
  [a1:561](../plans/a1-ruleset-architecture-map.md) — I-table rows recording
  what was verified at each node's writing. Left.
- **Conditional design statements** — a7 `:120, :186, :263, :359, :400,
  :561-562, :568-573, :586`; a6 `:192` — "under a triggered ECS host …"
  conditionals whose substance holds under the admitted host; the word
  "triggered" is now imprecise (the same cosmetic class #853 §3.1 assigns
  D44's wording). Left.
- **[a2:15-18](../plans/a2-kernel-game-module-ownership.md)** — rows stated
  to hold under any A3 outcome; true by construction. Left.
- **[ruleset-ecs-migration-brief.md](../plans/ruleset-ecs-migration-brief.md)**
  — the source brief whose hypothesis the #395 tree evaluated; hypothesis
  document, subordinate to D42. Left.
- **docs/spikes/\*** — dated snapshots by the spikes' own convention; #853
  §3.3 covered the two that cite the clause. Left.

## 4. Checked and excluded (false positives)

- **[a15:493](../plans/a15-occlusion-and-spatial-query-layering.md)** — "a
  trigger that has not fired" is A15 §7's *mechanic-driven* spatial-index
  trigger, not D42 (d)'s; no such mechanic has landed. True; excluded.
- **[docs/11-roadmap.md:65](../11-roadmap.md)** — "R-1's build-failure
  trigger has not fired" is the lightyear R-1 trigger. True; excluded.
- **a4 §6 matrix rows** (`:465-466`) — "probe-style CI test if Tier H lands"
  and E-M9 "proposed" are satisfied-conditional/proposed statuses, not false:
  E-M9's workers leg is not among the battery's five enforced clauses and
  has not landed. Left, noted here so the next reader does not re-sweep it.
- **docs/06-verifiable-core.md:428** — describes the client-side adapter
  pattern; not falsified by the host admission. Excluded.
- **docs/10-crates.md** and **docs/14-capacity.md** — already written
  against the current facts (the 2026-08-31 amendments and the §12.6/§12.7
  measurements); no correction needed. Verified, not assumed.

## 5. Out-of-scope staleness, flagged not corrected

Falsified by landings outside this sweep's trigger set; left for a future
sweep so this diff stays scoped to the two decisions and three landings:

- **a9:67, :263; a22:119** — "the `SimulationHost` seam … recommendation,
  not code" / "does not exist yet (E-15)" / "S5 has not been built" —
  falsified by #738 (S5's seam landing), which is not one of this sweep's
  three landings.
- **a10 §3's other fixture rows** — F-3 landed (#426); F-12's
  `gates/migration-bench` exists and `docs/plans/baselines/` holds
  `a18-baseline-2026-08-30.json`; F-2's status may have moved. Stale for
  reasons outside this sweep's facts.
- **a18 §2's audit table** — explicitly dated at `f82ee980` ("Status at
  HEAD" = that commit); several rows have moved since. Dated by its own
  convention; left.

## 6. Classification uncertainty, stated

- **a18:63 and a22:51-54** were the closest calls: both are factual claims
  embedded in argument sections. They are left under the no-argument-rewrite
  rule and named here so the owner can reverse that call in one edit if
  desired.
- **DECISIONS.md:57** is classified owner-reserved-adjacent (index of
  Accepted records, same edit as the ADR title per #853 §5) rather than
  correctable; if the owner prefers the index row refreshed as a doc chore,
  that is a one-line edit outside this branch.
- **a4 §6's "exists / conditional" cell** (`:465`) is left because the
  battery's landed probes are not verifiably the same instrument A4's row
  names (200-world probes); correcting it would assert more than the tree
  shows.
- **a7's "triggered" wording family** is left as cosmetic; a stricter
  reading would correct the word everywhere, which this lane judged to be
  rewriting conditional framing rather than correcting a false claim.

## 7. Verification

- `./scripts/lane-diff-audit.sh` — run on this diff; passes.
- `check.sh` — exempt: documentation-only (AGENTS.md).
- The diff touches five plan documents plus this summary; no ADR, no code,
  no gate, no golden, no spike other than this file.
