# Draft amendment — D21: `RETAINED_BUILDS` is one, and a superseded build is `Unadjudicable` by name

**Propose-only. Nothing is amended.** Amending an Accepted ADR is
owner-reserved ([DECISIONS.md](../DECISIONS.md)); this document drafts the
amendment text and the case for it so the owner can accept or reject it in
one reading. The diff in §4 is a proposal against
`docs/adr/0021-ruleset-distribution.md` as it stands; until the owner accepts
it, the record is exactly what it was, and this file changes nothing — and
**no code changes here either**: `crates/orrery_persistd/src/adjudication.rs:35`
still reads `pub const RETAINED_BUILDS: usize = 3`, and the implementing
change is named in §5 as what acceptance would authorise, not done.

Decision of record: [#880](https://github.com/baadc0de/orrery/issues/880),
"OWNER DECISIONS, 2026-09-02", item 2 — *"`RETAINED_BUILDS = 3` -> reduce to
1, and amend D21. … This needs a D21 amendment, drafted propose-only and
brought back for acceptance — not applied directly."* Documentation-only
lane: `check.sh` is exempt per [AGENTS.md](../../AGENTS.md);
`./scripts/lane-diff-audit.sh` was run and passes.

Everything below was read at source on this tree, with current line numbers.
Where an accepted record cites older line numbers for the constant
(`adjudication.rs:29`, `:33`, `:34`, `:324`), those were correct when written;
this document re-cites at today's numbering (`:35`) and says so.

---

## 1. What D21 says today, exactly

### 1.1 The number is stated four times, and frozen once

D21 never writes the digit `3` as a clause of its own. The horizon enters the
record in four places, each carrying it as a fact about the tree rather than
as a decision ([0021-ruleset-distribution.md](../adr/0021-ruleset-distribution.md)):

- **Context (`:22-23`):** *"the adjudication executor
  (`AdjudicationExecutor::register`, which holds `RETAINED_BUILDS` version-keyed
  workers)"*.
- **Decision 1, second bullet (`:71-73`):** *"The executor re-executes windows
  of up to 180 ticks per bundle and holds three concurrent ruleset builds. A
  sandbox tax there is paid per bundle, per build."*
- **Decision 2, the frozen-surface table (`:90`):** the row *"Rules on the
  evidence path | `AdjudicationExecutor::register`, `RETAINED_BUILDS`"* — the
  constant is a **frozen seam**, and *"a breaking change to the surfaces below
  requires an ADR that names this one"* (`:78-79`).
- **Decision 4 (`:103-107`):** the third reopen condition — *"a
  `RETAINED_BUILDS` horizon that proves too short in practice because redeploys
  are too expensive to be frequent."*

and once in Consequences (`:111-114`):

> **The game repo owns the persistd artifact**, and a rules change is a cluster
> deploy. Rolling deploys keep old builds alive for the adjudication retention
> horizon (three builds, D12); evidence older than that resolves as
> `Unadjudicable` — never a strike (D10).

### 1.2 What the record promises, read together

Put the five together and D21 promises this: during a rolling deploy, evidence
pinned to either of the two previous builds stays adjudicable somewhere in the
cluster, because the executor "holds three concurrent ruleset builds" and
"rolling deploys keep old builds alive". The promise is made to three parties
— the witness filing a report, the honest player whose provisional intent is
finalised later (D29), and the operator judging whether a hotfix cadence is
safe (Decision 4).

### 1.3 Where the promise breaks

The promise has two halves, and the tree delivers one of them.

**The executor half is real.** `AdjudicationExecutor::register`
(`adjudication.rs:418-428`) keeps a `VecDeque<Registered>` bounded at
`RETAINED_BUILDS` and pops the oldest past the cap; `retained()` lists them;
routing is by `RulesetId` on both the report path (`:469`) and the
provisional path (`:657-667`); an unknown id is
`Unadjudicable(UnknownRuleset)` and never a strike. Two tests keep the
eviction honest — `only_three_builds_stay_adjudicable` (`:1037`) and
`a_report_for_a_retired_build_is_undecidable_not_a_strike` (`:1045`) — and
[D49](../adr/0049-compatibility-manifest.md)'s M3 re-proved them by mutation.

**The deployment half is not.** A `persistd` binary links one version of
`orrery_games` ([D12](../adr/0012-backend-services.md) `:14`, D21 Context:
*"a game links its `Ruleset` and builds the `persistd` it deploys"*). One
binary therefore *has* one build to register, so one process registers one
build — the executor's capacity for three is never filled by anything the
architecture ships. And `gateway_report` routes in-process
(`gateway.rs`, the `adjudicator` field of `GatewayConfig`): nothing routes a
report to *another* process that might still hold the older binary. So
"rolling deploys keep old builds alive" is true of the processes and false of
the evidence — a report pinned to the previous build reaches whichever process
its session is on, and on an upgraded process it is `UnknownRuleset`.

#880 states this exactly (item 2): *"One binary links one version of
`orrery_games`, so one process registers one build. D21 says rolling deploys
keep old binaries alive, but `gateway_report` routes in-process and nothing
routes a report to the process holding the older build. Either three renamed
crate versions get linked, or evidence pinned to the previous build is
`Unadjudicable` for the whole rollout."*

Today the gap is also unobservable, for a reason that is #880's item 1 rather
than this amendment's: the shipped binary registers **no** build at all
(`gateway.rs:3183`: *"this crate ships the registration seam and registers
nothing"*), so every report is `REPORT_REFUSED_NO_ADJUDICATOR` and the
horizon question never arises. The owner deferred registration placement
until the freeze lifts. This amendment settles what the horizon *is* so that
when registration lands it lands against a record that is true.

### 1.4 What the constant's own comments say

Three attributions, three different records, none of them D21:

- `adjudication.rs:34-35`: *"How many rules builds the cluster keeps
  adjudicable at once (D16)."*
- `crates/orrery_protocol/src/verifiable.rs:577`: *"No retained build matches
  the bundle's `RulesetId` (D11 retains 3)."*
- D21 Consequences: *"(three builds, D12)"*.

D16's parameter table does carry the row (*"Ruleset builds retained
(adjudication) | 3"*, [0016 `:23`](../adr/0016-parameter-reference.md)); D12's
service inventory does say *"retains the last **3** ruleset builds"*
(`:14`); D11 says nothing about builds at all. The number has no single home,
which is part of why it could stay wrong. The amendment gives it one — D21,
the record that froze the seam — and names D12 and D16 as owed consequential
edits (§3.1).

---

## 2. The decision that happened, in the owner's words

### 2.1 The finding, 2026-09-01

#880 was opened from the persistence grooming pass with the horizon question
carried, not resolved: *"This is an ADR-level question (D21/D12), recorded
here, not decided."* Its acceptance evidence already contains the shape the
owner later chose — *"A report pinned to an unregistered build returns
`Unadjudicable(UnknownRuleset)` and files nothing."*

### 2.2 The decision, 2026-09-02

> **2. `RETAINED_BUILDS = 3` -> reduce to 1, and amend D21.**
> The architecture does one process, one registered build. Evidence pinned to
> a superseded build should return `Unadjudicable` **by name** during a rollout
> — a stated limit, not a silent gap. Linking three renamed crate versions, or
> routing reports across processes, both buy D21's literal promise at a cost
> out of proportion to it.
>
> This needs a **D21 amendment**, drafted propose-only and brought back for
> acceptance — not applied directly.

Three things in that text bind the draft:

- **"reduce to 1"** — the constant's value, not its existence. `RETAINED_BUILDS`
  stays a frozen seam; its value becomes the one the architecture can honour.
- **"`Unadjudicable` by name"** — the limit is stated in the record and
  surfaced by the tree: the verdict names its reason (`UnknownRuleset`), and
  the process names what it holds (`retained()` is already published in
  readiness, `bin/persistd.rs:1432-1434`). Nothing is silently dropped, and
  nothing is a strike.
- **"a cost out of proportion"** — the two alternatives are *rejected*, not
  deferred: neither linking renamed crate versions nor cross-process routing
  becomes a reopen condition. Decision 4's third condition is therefore
  restated (§4), because "a horizon that proves too short" can no longer be
  met by widening the horizon.

### 2.3 What the decision did not decide

Two of #880's carried questions stay open and this draft does not touch them:
**registration placement** (deferred until the freeze lifts — the owner's item
1) and the **`UniverseSeed` source** (item 3, *"still open"*). The amendment
is true whichever way those go.

---

## 3. Everything that depends on the horizon being three

The failure mode this section exists to prevent: an amendment that silently
changes what another record relies on. The test applied to each dependency is
**"does the restated clause make a live statement of another record false?"**
Unlike the D42 (d) case, the answer here is **not** *no* throughout: two
accepted records state the number as a fact of their own, and one accepted
record's honest-accounting sentence gets materially wider. Each is named
below with what it needs, and none is drafted here.

### 3.1 Accepted ADRs

| Record | What it says (quoted) | Disturbed? |
|---|---|---|
| [D12](../adr/0012-backend-services.md) `:14` | *"adjudication executor (retains the last **3** ruleset builds as version-keyed workers so evidence pinned to older rules stays adjudicable across hotfixes)"* | **Yes — a live statement becomes false.** The inventory row states the number and the purpose ("stays adjudicable across hotfixes"), and the purpose is exactly what the architecture cannot deliver. **Consequential amendment owed to D12**, one row, at acceptance: *"retains the one ruleset build it links; evidence pinned to a superseded build is `Unadjudicable(UnknownRuleset)` during a rollout, never a strike (D21, amended 2026-09-02)"*. Not drafted here — D12 is its own record. |
| [D16](../adr/0016-parameter-reference.md) `:23` | *"Ruleset builds retained (adjudication) \| 3"* | **Yes — a live parameter becomes false.** D16 is the parameter reference and the constant's own doc comment cites it. **Consequential amendment owed to D16**: the cell reads `1`, with the D21 pointer. Not drafted here. |
| [D10](../adr/0010-witnessing.md) `:11` | *"adjudicator-side failures (unavailable ruleset version, retention miss, oversize window) are merely* unadjudicable *— never a strike, rate-limited instead."* | **No — strengthened.** "Unavailable ruleset version" is now the *normal* state of a superseded build during a rollout rather than a rare one, and D10's rule is what makes the owner's "stated limit, not a silent gap" safe: it can never be a strike. Nothing in D10 says how many versions are available. |
| [D29](../adr/0029-low-population-path.md) `:451-452`, `:466-469` | *"The existing executor is a pure router over retained builds (`adjudication.rs:44-51`, `RETAINED_BUILDS = 3` at `:29`)"*; *"Sharing the executor is deliberate: `RETAINED_BUILDS = 3` is the scarce resource, and two registries would give two answers to 'which build adjudicates this window'"* | **No — the normative content survives, the number is stale.** The clause decides that provisional finalisation shares the executor and creates no second registry; that reasoning holds *a fortiori* at one build (two registries could disagree about one build as easily as about three). The digit in the citation is cosmetic. |
| [D29](../adr/0029-low-population-path.md) Consequences `:674-678` | *"`RETAINED_BUILDS = 3` (`adjudication.rs:29`) continues to bound both workloads, which means a provisional intent pinning a build older than the last three finalizes as `Unadjudicable` → annulled with no strike. That is a new way for an honest player to lose an item during a rules upgrade…"* | **Materially wider, and the record must say so.** The sentence stays true with "the last three" read as "the one build the process holds" — but the window it describes grows from *older than two releases* to *the previous release*, i.e. every provisional intent pinned to the pre-rollout build and finalised on an upgraded process is annulled. D29 already flags this as *"the asymmetry D29 flags for review"* (`adjudication.rs:624-625`); the amendment does not create the trade, it widens the population that takes it. **Named for the owner as a consequence to accept knowingly** (§5); the mitigation — finalising provisional intents *before* an upgrade drains a process, or holding finalisation until the pinned build is back — is an operational rule for the rolling-deploy runbook and P8, not a D29 rewrite. Not drafted here. |
| [D38](../adr/0038-at-rest-schema-versioning.md) `:70-73`, `:200-204` | *"holds version-keyed builds bounded by `RETAINED_BUILDS = 3` (`adjudication.rs:324`, `:34`)"*; *"`RETAINED_BUILDS` bounds adjudication evidence, not schemas"* | **No.** The first is a Context observation that the registration pattern exists inside the frozen surface — still true at any value. The second is the orthogonality rule, and it is stated without a number. The digit in the citation is cosmetic. |
| [D48](../adr/0048-canonical-witness-projection.md) `:218` | *"`RETAINED_BUILDS` still bounds adjudication evidence, not schemas and not projections."* | **No.** Stated without a number; the orthogonality survives. |
| [D49](../adr/0049-compatibility-manifest.md) `:99`, `:198-199`, `:500` | *"`RETAINED_BUILDS = 3` bounds adjudicable builds and eviction is real"*; *"unlike evidence rows it never ages out with the `RETAINED_BUILDS = 3` horizon, because it is the decoder ring for every older row"*; M3's mutation row naming `only_three_builds_stay_adjudicable` | **No — the argument gets stronger.** D49 makes the manifest record permanent *because* the horizon is shorter than the evidence it must decode; a horizon of one makes that reason more, not less, compelling. The M3 row is a dated verification record naming a test that the implementing change renames (§5); dated records are not re-run by amendment. |
| D21 itself, Decision 2 (`:78-79`) | *"a breaking change to the surfaces below requires an ADR that names this one, not a patch release."* | **This draft is that record.** Changing a frozen constant's value is a change to a frozen seam's contract even though no signature moves; the amendment names D21 by construction, and §4 says so in the text so a reader does not have to infer it. |

Not a dependency, checked and excluded: [D32](../adr/0032-enforcement-ramp.md)
mentions no horizon — C3/C4/C5 consume verdicts and are indifferent to how a
verdict became `Unadjudicable`; [D11](../adr/0011-persistence.md) says nothing
about retained builds despite `verifiable.rs:577` attributing the number to it
(a comment defect, §3.3).

### 3.2 Plan and design documents (subordinate; the ADR wins)

| Record | What it says | Disturbed? |
|---|---|---|
| [docs/11-roadmap.md](../11-roadmap.md) `:412` | P4 crates: *"`orrery_persistd` (adjudication executor linking the same `Ruleset`) — **landed** as version-keyed routing over the 3 retained builds"* | **Stale digit in a landed-status note.** Routing is version-keyed and landed; the count is the constant's. Doc chore at acceptance. |
| [docs/11-roadmap.md](../11-roadmap.md) `:1101-1105`, `:1114-1116` | P8 (proposed, not accepted): *"D21's three reopen conditions — … a `RETAINED_BUILDS` horizon too short because redeploys are too expensive — are all live-patch observations"*; *"`orrery_persistd` (rolling deploy across the `RETAINED_BUILDS = 3` adjudication horizon…)"* | **P8's rolling-deploy content must be re-read, not just re-numbered.** P8's goal is *"zero skew-caused strikes"*, which holds (D10: never a strike). What a horizon of one does **not** give P8 is *zero skew-caused annulments*: the D29 widening in §3.1 is exactly a P8 observation. And the third reopen condition P8 lists is restated by §4 — widening the horizon is no longer the remedy. P8 is proposed text, so this is a note for its author, not an amendment. |
| [docs/07-witnessing.md](../07-witnessing.md) `:142-143` | `Verdict` sketch: *"ruleset version outside the 3 retained builds, retention miss, oversize window -> NO strike"* | **Stale digit.** Doc chore at acceptance; the semantics (no strike) are the ones the amendment relies on. |
| [docs/08-persistence.md](../08-persistence.md) `:4128` | *"`RETAINED_BUILDS` bounds adjudication evidence, not schemas."* | **No.** Stated without a number. |
| [A1](../plans/a1-ruleset-architecture-map.md) `:203`, `:442`, `:588`; [A8](../plans/a8-compatibility-manifests.md) I10, M-A8-3; [A11](../plans/a11-adrs-and-pr-plan.md) `:157`; [A7](../plans/a7-persistence-rollback-witnessing.md) `:523` | Architecture map, invariant tables, and mutation records citing `RETAINED_BUILDS = 3` and the two named tests. | **No — history.** Dated audits and plans; A8 I10 is the invariant the amendment keeps (*"a report for a retired build is `Unadjudicable(UnknownRuleset)`, never a strike"*) with the count changed. Not re-run by amendment. |

### 3.3 Code and scripts

| Location | What it says | Disturbed? |
|---|---|---|
| `crates/orrery_persistd/src/adjudication.rs:34-35` | `/// How many rules builds the cluster keeps adjudicable at once (D16).` / `pub const RETAINED_BUILDS: usize = 3;` | **The implementing change** (§5): value `1`, comment cites D21 (amended) with D16 as the parameter row. Not done here. |
| `adjudication.rs:363-365` | *"Newest last. Bounded at [`RETAINED_BUILDS`]; registering a fourth retires the oldest, which is what makes `UnknownRuleset` reachable and therefore worth testing."* | **Comment stale at value 1** ("a second retires the first"); the mechanism — `retain` by id, `push_back`, pop past the cap — is unchanged and needs no code edit. |
| `adjudication.rs:609-614`, `intent/provisional.rs:354`, `:389`, `:537` | *"[`RETAINED_BUILDS`] is the scarce resource, and two registries would give two answers…"* | **No.** Stated without a number; the shared-executor reasoning holds at any value. |
| `adjudication.rs:662-667` | *"an intent pinning a build older than the last three is annulled with nobody at fault"* | **Comment stale**; the code path is exactly the one the owner chose (`unknown_ruleset_verdict()`, never a strike). The D29 widening (§3.1) is this line's. |
| `adjudication.rs:1037-1043` `only_three_builds_stay_adjudicable`; `:1045-1055` `a_report_for_a_retired_build_is_undecidable_not_a_strike`; `:1057-1071` `a_report_is_routed_to_the_build_its_subject_ran` | The first asserts `[2, 3, 4]` retained after a fourth registration; the third asserts V1–V3 all adjudicable. | **Both fail at value 1 by construction** — the tests are the seam's proof and they prove the old number. The implementing change rewrites them to the new horizon (one retained; a second registration retires the first; a report pinned to the retired build is `UnknownRuleset`) and keeps the names D49 M3 relies on *or* records the rename. The second test survives unchanged. |
| `crates/orrery_protocol/src/verifiable.rs:577` | *"No retained build matches the bundle's `RulesetId` (D11 retains 3)."* | **Comment defect, independent of value**: D11 retains nothing. Fix in the implementing change to cite D21. |
| `crates/orrery_persistd/src/bin/persistd.rs:1432-1434`, `:1572` | Readiness publishes `retained()` and `adjudicator_configured`. | **No — this is the "by name" surface.** A process states which build it holds; a superseded-build refusal is attributable to that statement. |

**Summary:** the restated record falsifies two live statements — D12's
inventory row and D16's parameter cell — and materially widens one honest
accounting, D29's annulment-on-upgrade sentence. All three are named for the
owner as consequential edits or knowing acceptances; none is drafted here.
Everything else that cites the number is either stated without it, made
stronger by a shorter horizon (D49), or a dated record.

---

## 4. The drafted amendment

Proposed changes to `docs/adr/0021-ruleset-distribution.md`. D21 marks
corrections as dated blockquotes against the paragraph they correct (the
2026-08-30 `validate_intent` correction at `:26-41` is the precedent), and
that convention is followed: the original text stays readable, the amendment
is recorded where it bites, and one new numbered clause carries the decision.
The amendment note's status line is drafted as it would read once accepted;
**until the owner accepts, it is not in force and the tree is unchanged.**

```diff
 **Status:** Accepted · **Date:** 2026-08-20 · **Decision:** D21
+
+**Amended 2026-09-02 (owner-authorised, [#880] item 2).** The adjudication
+retention horizon `RETAINED_BUILDS` is **one build**, not three: a `persistd`
+process links one `Ruleset` and registers one build, and nothing in the
+architecture routes evidence to a process holding another. Decision 5 carries
+the restatement; the Decision 1 bullet, the Consequences bullet and reopen
+condition 4(iii) that stated three are corrected against their text below.
+This is the ADR Decision 2 requires for a change to a frozen seam's contract.
```

```diff
 - **Adjudication performance is on the enforcement path.** The executor
   re-executes windows of up to 180 ticks per bundle and holds three concurrent
   ruleset builds. A sandbox tax there is paid per bundle, per build.
+
+  > **Corrected 2026-09-02 ([#880]).** One build, per Decision 5. The
+  > argument is unchanged and slightly stronger: the sandbox tax would still
+  > be paid per bundle on the enforcement path, and the reason for keeping
+  > rules in process — one binary, one build, no second execution environment
+  > to prove deterministic — is the same reason there is one build to hold.
```

```diff
 **4. What reopens the question.** Concrete demand, in one of these forms: a
 title that must run rules it does not compile; an operational requirement to
 ship a rules hotfix without a persistd redeploy that a rolling deploy cannot
 meet; or a `RETAINED_BUILDS` horizon that proves too short in practice because
 redeploys are too expensive to be frequent. Absent one of those, this is
 settled for 1.0.
+
+> **Amended 2026-09-02 ([#880]).** The third condition is restated. Under
+> Decision 5 the horizon is one build by construction, and the two ways of
+> widening it — linking several renamed versions of the game crate into one
+> binary, or routing evidence across processes to whichever still holds the
+> pinned build — were rejected by the owner as buying the literal promise at a
+> cost out of proportion to it. "Too short" therefore no longer reopens the
+> horizon; it reopens *this* decision in the form the first two conditions
+> already name: demand for rules the binary does not compile, or a hotfix
+> cadence the rolling deploy cannot carry without an unacceptable
+> `Unadjudicable` window. The evidence for either is a measured window (the
+> `UnknownRuleset` rate across a rollout), not an argument about the constant.
+
+**5. The adjudication horizon is one build, and a superseded build is refused
+by name.** `RETAINED_BUILDS = 1`. A `persistd` process holds exactly the
+build it was compiled with, registers it once, and publishes it in readiness
+(`retained()`). During a rolling deploy, a report or a provisional
+finalisation pinned to any other `RulesetId` — the build being retired, or a
+build the process has never held — resolves as
+`Unadjudicable(UnknownRuleset)`: named by its reason, attributable to the
+process's published build, counted, and **never a strike** (D10). This is a
+stated limit of the distribution model Decision 1 chose, not a gap in it:
+the same fact that makes link-time composition deterministic — one binary,
+one build — is what bounds the horizon at one.
+
+What the clause does *not* do: it does not change `AdjudicationExecutor::
+register`'s shape (`retain` by id, append, retire past the cap — a second
+registration retires the first), it does not add a second registry (D29's
+shared-executor rule stands), and it does not make any evidence
+*adjudicable* that was not: it makes the record say what the tree does. The
+two rejected widenings are rejected, not deferred; a future case for either
+is a new record that names this clause.
```

```diff
 - **The game repo owns the persistd artifact**, and a rules change is a cluster
   deploy. Rolling deploys keep old builds alive for the adjudication retention
   horizon (three builds, D12); evidence older than that resolves as
   `Unadjudicable` — never a strike (D10).
+
+  > **Corrected 2026-09-02 ([#880]).** Rolling deploys keep old *processes*
+  > alive, and each holds one build (Decision 5); evidence reaches the process
+  > its session is on, not the process holding its pinned build. So during a
+  > rollout, evidence pinned to the retiring build resolves as
+  > `Unadjudicable(UnknownRuleset)` on every upgraded process — never a strike
+  > (D10) — and a provisional intent pinned to it and finalised there is
+  > annulled with nobody at fault (D29 Consequences), a window that is now the
+  > width of one release rather than two. The D12 inventory row and the D16
+  > parameter cell that carried "3" are owed the same correction through their
+  > own records.
```

```diff
 [D19](0019-indexed-waldb-journal.md) landed the backend that made the P2 gate
 hold.
+
+[#880]: https://github.com/baadc0de/orrery/issues/880
```

Provenance notes on the restatement, so the owner can check it against the
records in one pass:

- **Nothing is decided that #880 did not decide.** Decision 5's two operative
  sentences are the owner's *"one process, one registered build"* and
  *"`Unadjudicable` by name during a rollout — a stated limit, not a silent
  gap"*, and the two rejected widenings are the owner's two rejections (§2.2).
- **"By name" is given a concrete surface**, not left as a slogan: the verdict
  carries `UnknownRuleset` (`verifiable.rs:576-578`), and the process publishes
  what it holds (`bin/persistd.rs:1432-1434`). Both already exist.
- **The D10 rule is load-bearing** and is cited at every point the limit is
  stated: it is what makes a stated limit safe to state.
- **Decision 2's freeze is honoured explicitly.** The value of a frozen
  constant changes; the record says this is the ADR the freeze demands rather
  than leaving a reader to decide whether a value change is "breaking".
- **The D29 widening is stated in the Consequences correction** rather than
  left to be discovered by composing three records.
- **New record reference:** `[#880]` needs adding to D21's link table, which
  currently has none — the draft adds it at the end of the header.

---

## 5. What the amendment does not do, and what acceptance would authorise

- **It does not change code.** `adjudication.rs:35` stays `3` until the
  amendment is accepted. What acceptance authorises, as one small change in
  `orrery_persistd` (a frozen crate, but this is the ADR the freeze requires):
  the constant to `1` with its comment citing D21; the two stale comments
  (`:363-365`, `:662-667`) and the `verifiable.rs:577` misattribution;
  and the three tests in §3.3 rewritten to the new horizon — one retained, a
  second registration retires the first, the retired build is `UnknownRuleset`
  — keeping `a_report_for_a_retired_build_is_undecidable_not_a_strike`
  verbatim. `only_three_builds_stay_adjudicable` becomes
  `only_one_build_stays_adjudicable`; D49 M3's row names the old test and is a
  dated record.
- **It does not light the adjudicator.** #880 item 1 (registration placement)
  is deferred until the freeze lifts, by the owner. Until it lands, the
  shipped binary refuses every report as `REPORT_REFUSED_NO_ADJUDICATOR` and
  the horizon is moot in production. The end-to-end evidence the owner asked
  for — *a report pinned to a superseded build returns `Unadjudicable` by name
  during a rollout, against the binary* — is therefore provable at the
  library level now (the rewritten tests) and at the binary level only after
  registration lands. The draft says so rather than claiming the binary proof.
- **It does not decide the `UniverseSeed` source** (#880 item 3).
- **It does not amend D12 or D16.** Both carry the number as a live statement
  and are owed one-line consequential edits through their own records (§3.1);
  the Consequences correction names them so the debt is visible from D21.
- **It does not rewrite D29.** The annulment-on-upgrade window widens as a
  consequence and is stated where it bites (§4, Consequences); the operational
  mitigation — drain provisional finalisation before a process is upgraded, or
  hold finalisation until the pinned build is reachable again — is a
  rolling-deploy runbook and P8 concern, named for the owner and not designed
  here.
- **It does not reopen Decision 1.** Link-time composition is the *reason* the
  horizon is one; the amendment makes the record consistent with it rather
  than arguing against it.
- **Nothing operational changes on acceptance alone.** No verdict that is
  reachable today becomes unreachable; no evidence that is adjudicable today
  stops being adjudicable (nothing is registered); no gate, allowlist or scan
  moves. `./scripts/lane-diff-audit.sh` passes and no code file is in this
  diff.

---

## 6. Verification

- The quoted D21 text was read at `docs/adr/0021-ruleset-distribution.md`
  `:22-23`, `:71-73`, `:78-79`, `:90`, `:103-107`, `:111-114` on this tree.
- `RETAINED_BUILDS` sites were enumerated with a repository-wide search over
  `*.md`, `*.rs`, `*.sh` and `*.toml`; every hit is classified in §3, and the
  constant, its comment and its three tests were read at
  `crates/orrery_persistd/src/adjudication.rs:34-35`, `:363-365`, `:418-428`,
  `:657-667`, `:1037-1071`.
- The owner's decision was read from #880's "OWNER DECISIONS, 2026-09-02"
  comment and is quoted verbatim in §2.2.
- Documentation-only; `check.sh` exempt. `./scripts/lane-diff-audit.sh` run
  on the branch that carries this file.
