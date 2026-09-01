# Draft amendment — D42 clause (d): the record of an admission, restated

**Propose-only. Nothing is amended.** Amending an Accepted ADR is
owner-reserved ([AGENTS.md](../../AGENTS.md), [DECISIONS.md](../DECISIONS.md));
this document drafts the amendment text and the case for it so the owner can
accept or reject it in one reading. The diff in §4 is a proposal against
`docs/adr/0042-canonical-simulation-architecture.md` as it stands; until the
owner accepts it, the record is exactly what it was, and this file changes
nothing.

Debt of record: [#745](https://github.com/baadc0de/orrery/issues/745), item 1
of "Two things this does not discharge" — *"D42 clause (d) still reads as
trigger-gated"*. Documentation-only lane: `check.sh` is exempt per
[AGENTS.md](../../AGENTS.md) (prose and ADR work need no lane);
`./scripts/lane-diff-audit.sh` was run and passes.

Everything below was read at source on this tree, with current line numbers.
Where an issue comment cites older line numbers (`D42:237-238`, `D43:346`),
those were correct before #759/#805 lengthened the files; this document
re-cites at today's numbering and says so.

---

## 1. What clause (d) says today, exactly

### 1.1 The heading and the operative body

The clause opens under a heading that states the trigger-gated rule
([0042-canonical-simulation-architecture.md:338](../adr/0042-canonical-simulation-architecture.md)):

> ### (d) A dedicated `bevy_ecs::World` is admitted only behind the host seam, on pre-registered triggers

and its operative body (`:381-384`) is still the original 2026-08-25 text:

> A dedicated canonical `bevy_ecs::World` — same per-entity topology, different
> substrate behind the seam — is neither adopted nor foreclosed. It becomes a
> legal *future* host implementation, adoptable only when a pre-registered
> trigger fires:

### 1.2 What states the triggers T1/T2/T3 (`:386-394`)

> - **T1:** a shipped module genuinely needs per-component canonical storage
>   with independent per-component policies — i.e. `CoreState`-as-one-enum
>   measurably stops scaling.
> - **T2:** measured tick cost in a real host shows the `BTreeMap` store
>   dominating.
> - **T3:** A4's Tier-H gate bundle lands and is demonstrated — mutation-style
>   — at least as strong as the clauses it replaces; a weaker bundle fails the
>   programme's standing bar ("a weaker gate that passes is worse") and must
>   not ship.

followed by (`:396-402`):

> T3 is a necessary precondition for any canonical byte to move regardless of
> T1/T2, per the pilot precondition list both lanes carry (A3 §7.4): the gate
> bundle, the differential parity harness beyond goldens, capacity-scale
> mirror-cost numbers replacing the indicative P4/P-1 bounds, and A5's
> component-policy registry. The trigger *definitions* may be amended by the
> owner (second opinion §3 V5 marks them "owner may amend"); amendment is an
> owner decision recorded against this clause, not a silent edit.

### 1.3 The Bevy-free floor, still written as trigger-conditional (`:404-410`)

> **Until a trigger fires, the gated crates stay Bevy-free and
> `scripts/core-gates.sh` clause 1 stays exactly as it is** — the Bevy-free
> scan over `GATED_CRATES=(orrery_core orrery_games orrery_conformance)`
> (`scripts/core-gates.sh:37,66-77`). Nothing replaces the gate because
> nothing weakens it. (R2's Tier V discovery clause *strengthens* membership
> detection and is compatible with this sentence; the conditional Tier H
> battery arms only with a trigger here.)

### 1.4 What the clause licenses, and what it withholds

**Licensed by the body alone:** a dedicated `bevy_ecs::World` as a *future*
host implementation, and only once T1, T2 or T3 has fired. **Withheld by the
body alone:** any adoption without a trigger — including the two adoptions
that in fact happened — and a Bevy dependency for all three gated crates,
`orrery_games` included.

The record knows this and carries two amendment blockquotes on top of that
body. The first (`:340-379`, recorded by #759) records the admission of the
host and reinterprets the trigger list — *"Read the trigger list as the
conditions under which adoption would have been* automatic*; the owner
retains the separate power to admit a host directly, which is what happened"*
— with a package ledger and T3's narrowed status. The second (`:412-425`,
recorded by #805) narrows the Bevy-free floor — *"The set the sentence
protects is now `orrery_core` and `orrery_conformance`, and for those two it
stands unweakened."*

Two internal inconsistencies follow from the layering, both visible without
any external fact:

- the ledger table (`:359`) still reads "**not met** — see the open question
  below" for the Tier-H gate bundle, while the nested note inside the same
  blockquote (`:367-373`) records it **discharged** for the admitted host;
- the body's "*the conditional Tier H battery arms only with a trigger here*"
  (`:409-410`) describes a mechanism that no longer exists — the battery arms
  per declared host through `DECLARED_HOST_CRATES`
  (`scripts/core-gates.sh:510`), as [D43](../adr/0043-determinism-envelope-and-gate-replacement.md)
  clause (e)'s amendments record.

So the precise state is: **the events are recorded against the clause; the
rule is not.** The clause now contains two of its own overrides. A reader who
applies the body — which is the natural reading order — licenses something
different from what governs, and the record's own recent history shows
readers doing exactly that (§2.3).

---

## 2. The decisions that actually happened, in the owner's words

### 2.1 The sanction, 2026-08-30

[#745](https://github.com/baadc0de/orrery/issues/745), the issue body:

> **Owner sanction, 2026-08-30:** `bevy_ecs` is approved and S7 may start. No
> D42 (d) trigger fired; the owner sanctioned entry directly.

**Scope, not paraphrased wider:** this sanctioned *S7 entry* — the
A18-programme lane whose acceptance instrument was the four-class F-4
differential harness. It did not license putting `bevy_ecs` into a gated
crate, and the first attempt to do exactly that was refused by the record and
then withdrawn by the owner's own lane (§2.3). The issue body itself names the
two things the sanction does not discharge, the first being: *"D42 clause (d)
still reads as trigger-gated — it admits a dedicated `bevy_ecs::World` behind
the seam* only *on T1/T2/T3. The record should be amended to match the
decision rather than the decision diverging from it. Amending an Accepted
record is owner-reserved; naming it as owed, not doing it."*

### 2.2 The landing, 2026-08-31, and the debt restated

The host landed as `orrery_sim_host`'s `EcsBackend` (#757). #745's comment on
the landing:

> **S7.4 has landed — #757.** `bevy_ecs` is in the tree, at the seam, at
> four-class F-4 parity, with `core-gates.sh` still at exit 0 and no ADR
> amendment required. … Both undischarged items from the sanction still stand:
> **D42 clause (d) still reads as trigger-gated** and should be amended to
> match the decision rather than diverge from it, and **Tier H is still
> empty**.

The same thread's audit comment verified the divergence at source and listed
what an amendment must close, in the owner's words:

> The record prescribes its own mechanism — **D42:241-243**, "amendment is an
> owner decision **recorded against this clause**, not a silent edit." Four
> points:
>
> 1. **The admission gate.** "Adoptable only when a pre-registered trigger
>    fires" versus a host admitted by direct sanction.
> 2. **The package ledger** — per item: waived, re-dated, or still owed; and
>    if owed, owed *before any further adoption* or as follow-up. Note
>    D43:339 forbids the follow-up reading.
> 3. **T3's status** — still a gate for anything not yet built, retired, or
>    re-registered as an evidence obligation for the new host.
> 4. **D42:151-154's stale sentence.**
>
> A18 §5's S7 text and A3 §7's block will read stale afterwards and should be
> named in the decision; both are plan docs, subordinate to the ADR.
>
> **I am not drafting the amendment** — that is owner-reserved. This is the
> gap, quoted, so it can be closed in one edit.

Point 4 was already closed by clause (a)'s first 2026-08-31 amendment (the
store-of-record sentence now names the seam, not one store). Points 1–3 are
clause (d) and are what §4 drafts.

### 2.3 The divergence had teeth while it lasted

The first S7.4 attempt migrated `regolith.world` inside the gated crate. It
reached four-class parity and then hit the record's own body, applied as live
law — #745's verdict comment, quoting the clause at source:

> D42 clause (d), read at source:
>
> > **Until a trigger fires, the gated crates stay Bevy-free and
> > `scripts/core-gates.sh` clause 1 stays exactly as it is** (`docs/adr/0042-canonical-simulation-architecture.md:245`)
>
> and at `:283`, that proposals to put `bevy_ecs` in a gated crate **"are
> refused by this record, not argued case-by-case."**

with the recommendation:

> **Do not land, and do not amend D42 (d) on this evidence.** The record's
> refusal is doing its job, and the measurement independently argues the same
> way…

That refusal was correct under the sanction's scope (§2.1) — and it is also
the demonstration that the clause's body was being read as the governing rule
while the governing fact was the owner's direct power. A record that requires
each reader to reconcile a body with its own blockquotes will keep producing
this either way: a refusal that cites superseded law, or an adoption that
treats direct sanction as generally available when the record has never said
what it requires. Both happened within one week on this tree.

### 2.4 The acceptance, 2026-08-31 — a second, separate, broader decision

[#793](https://github.com/baadc0de/orrery/issues/793), comment **"OWNER
ACCEPTANCE, 2026-08-31"**, in the owner's words:

> I accept the amendments. Bevy ECS in `orrery_games` as first class
> dependency. ECS as idiomatic storage for entities and ruleset logic.
> Systems registration and tick driving works over `World`.

with the scope guard in the same comment:

> **D42 (a)'s core claim survives in substance**: canonical truth still does
> not live in a Bevy *application* world. What is admitted is a dedicated
> `bevy_ecs::World`, now reachable from the rules themselves rather than only
> from the host.

and the later correction, which matters for what this amendment may claim:

> **Correction: the dependency was accepted but never taken.** … #805 wrote
> the amendments into D42 (a), D42 (b)(2)/(d), D43 (d)/(e)(1) and added
> `orrery_games` to `BEVY_PERMITTED_CRATES`, so the gate now **permits** the
> dependency while keeping the crate gated on every other clause.

Verified on this tree: `crates/orrery_games/Cargo.toml` declares
`orrery_core`, `orrery_protocol`, `orrery_compose`, `libm`,
`rand_chacha` and `blake3` — no Bevy — and
`scripts/core-gates.sh:259` carries `BEVY_PERMITTED_CRATES=(orrery_games)`.

### 2.5 The owner's own disposition of the debt

#745's final comment:

> **The two items in this issue's "does not discharge" list are both now
> resolved:** D42 (d)'s trigger-gating is overtaken by direct acceptance, and
> Tier H landed in #771.

That is the owner's disposition of the *question*, and this draft does not
relitigate it. What it addresses is narrower: **"overtaken" is a history, not
a rule.** The clause's body still prescribes the trigger-gated license; the
decisions of record are two exercises of the owner's recorded power around
it. §4 restates the body so the rule and the history agree, which is what
#745 item 1 asked for from the start — *"amended to match the decision rather
than the decision diverging from it."*

---

## 3. Everything that depends on (d) reading trigger-gated

The failure mode this section exists to prevent: an amendment that silently
changes what another record relies on. The test applied to each dependency is
**"does the restated clause make a live statement of another record false?"**
The answer is *no* throughout, for one structural reason: the decisions of
2026-08-30/31 already disturbed every one of these texts in fact — the host
exists, the permission exists. The amendment does not newly disturb anything;
it removes the textual contradiction those decisions created. Dependencies
are grouped by what kind of reliance they have.

### 3.1 Accepted ADRs

| Record | What it says (quoted) | Disturbed? |
|---|---|---|
| [D43](../adr/0043-determinism-envelope-and-gate-replacement.md) (e), heading `:332-334` | *"Tier H — conditional host battery, armed only by a D42 trigger … Tier H exists only if [D42]'s dedicated-world trigger (T1–T3) ever fires."* | **No.** Already reconciled by D43's own 2026-08-31 amendments (`:355-421`): admission recorded, debt paid, confinement lifted, `orrery_games` made *eligible*. The restated (d) keeps every mechanism those amendments rely on — the trigger list, the battery, the allowlist. Note for the owner: D43 (e)'s heading and its *"Until a trigger fires, Tier H is empty"* honest-accounting paragraph (`:519-526`) carry the same body-vs-blockquote layering (e)(1)'s third amendment already re-describes as *"the tree rather than indicting it"*. A companion D43 restatement is a separate, recommendable follow-up — **not drafted here** (§5). |
| [D44](../adr/0044-identity-classes-and-allocation.md) (b) `:222-225` | *"For any present or future world that mirrors canonical entities — including a triggered dedicated ECS world under [D42] clause (d) (A5 N-2)"* | **No.** Forward-conditional; the admitted `EcsBackend` world is exactly a world this clause governs. The word "triggered" goes loose — the world was admitted, not triggered — which is cosmetic; the normative content (one index, host-owned, never canonical) is untouched. |
| [D47](../adr/0047-rollback-unit.md) (b) `:163-169` | *"The unit does not change under a triggered ECS host. If a [D42] trigger ever admits a dedicated world, its rollback substrate is a per-entity ring…"* | **No.** Same shape: conditional-forward, and the admitted host satisfies the condition's *substance* (per-entity `TickBackend`, `PersistId`-keyed; no world snapshot exists to take). Cosmetic staleness only. |
| D42 (b), layering `:310-314` | *"…the gated crates' dependency posture is unchanged by construction — see clause (d)."* | **No.** The posture change `orrery_games` took was recorded against (d) by #805; the restatement carries that recording forward, so the cross-reference stays true. |
| D42 (e) `:427-437` | *"…the pilot preconditions of clause (d) stay binding on the hybrid's ECS-hosted tier regardless."* | **No.** The restated clause keeps the precondition package riding the admission (§4, ledger) — the sentence stays true and keeps its force. |
| D42, Consequences `:441-462` | *"…clause (d) — pre-registering what would justify going further"*; *"Until an R1 trigger fires, proposals to put `bevy_ecs` in a gated crate are refused by this record…"* (with the 2026-08-31 parenthetical re-scoping to `orrery_core`/`orrery_conformance`) | **No.** The restatement keeps both true: the trigger list is retained as the automatic path ("what would justify going further" without an owner decision), and the refusal of the still-scanned crates is restated in the floor paragraph. The bullet's *"Until an R1 trigger fires"* opening is now decorative rather than load-bearing — flag for the owner as an optional same-edit touch-up, not drafted. |
| D42, title `:1` and Context §2 `:80-84` | *"…and a dedicated ECS world is trigger-gated"*; *"admit a dedicated `bevy_ecs::World` only later, behind the seam, on pre-registered conditions"* | **Title: stale under the amendment; Context: no.** Context §2 is the reasoning history of the original decision and is never rewritten by amendment. The title is the record's name and would contradict a restated (d); changing it is the owner's call and is **named, not drafted** (§5) — the index row already carries the amendment history, which is how D42's (a) amendments were handled without a retitle. |

Not a dependency, checked and excluded: [D46](../adr/0046-message-class-semantics.md) `:314-318`'s *"Clauses (a)–(d) ratify semantics the tree already exhibits"* refers to **D46's own** clauses (a)–(d), not D42's. D45 and D49 sit under D42 as umbrella but neither relies on (d)'s admission mechanism; D49 `:30-31` lists D42 as out of scope with D42 as its owner.

### 3.2 Plan and measurement documents (subordinate; the ADR wins)

| Record | What it says | Disturbed? |
|---|---|---|
| [A18 §5 S7](../plans/a18-ruleset-ecs-implementation-programme.md) `:488-507` | *"D42 clause (d) admits a dedicated `bevy_ecs::World` behind the seam only on pre-registered triggers T1 … T2 … T3 … **T3 is a necessary precondition regardless of T1/T2.** None has fired; Tier H is empty by D43 clause (e)'s own accounting."* | **Already false in fact before this amendment; no new disturbance.** This is the text #745's item 2 leans on, and both of its factual claims are overtaken (host admitted; Tier H landed, #771). The #745 audit comment anticipated exactly this: *"A18 §5's S7 text and A3 §7's block will read stale afterwards and should be named in the decision."* Recommendation in §6: refresh in the accepting edit or as a doc chore; do not fold into the (d) diff. |
| [A22](../plans/a22-engine-agnostic-client.md) `:13-14, :41, :45-49, :140, :196` | *"D42 clause (d) trigger-gates that and no trigger has fired"*; *"S7 — D42 (d) trigger: T1, T2 or T3"*; track E *"only on a fired D42 (d) trigger"*; §7 reserves *"Any ECS adoption - D42 (d), unchanged and untouched here."* | **Already false in fact; no new disturbance.** A22 was written 2026-08-30 against `0c01d4e` and states the body's rule. Its §7 reservation (ECS adoption stays owner-reserved) is *strengthened*, not weakened, by the restatement — the restated clause says the owner's power is exercised by recorded decision, which is what A22's reservation assumed. Named for the same doc-chore follow-up as A18 §5. |
| [A11](../plans/a11-adrs-and-pr-plan.md) `:57, :83, :162, :439-474` | The planning proposal that became D42; trigger-gated language throughout R1's text. | **No — history of the decision, not a live rule.** Never touched by amendment; the ADR is normative over it. |
| [A3](../plans/a3-simulation-host-comparison.md) §7.4 / [second opinion §3 V5](../plans/a3-simulation-host-second-opinion.md) `:330-360, :450-460` | Source of the trigger candidates and the precondition package; second opinion marks them *"owner may amend"*. | **No — history.** The restatement leans on this text (the triggers' provenance, the "owner may amend" reservation) and changes nothing in it. |
| [A12](../plans/a12-exchange-systems-shakedown.md) `:23-32` | Quotes D42 as *"a dedicated `bevy_ecs::World` is admitted only behind pre-registered triggers"* in its staleness audit. | **No — dated incident snapshot.** Its quote was accurate when written; incident records do not track later amendments. |
| [docs/14-capacity.md](../14-capacity.md) §12 `:1326-1341, :1427, :1474-1486` | *"D42 clause (d)'s last unmet precondition asked for capacity-scale mirror-cost numbers"*; *"does not demonstrate D42's T2"*; §12.6's *"partly discharged"* judgement. | **No — and the restatement depends on it.** These paragraphs read (d) through its admission record, which the restatement carries forward verbatim in the ledger; T2 stays defined, so `:1427`'s finding stays meaningful. |

### 3.3 Code and scripts (comments citing the clause)

| Location | What it says | Disturbed? |
|---|---|---|
| `crates/orrery_core/src/executor.rs:617-621` | *"[`Executor`] is the reference implementation and the canonical store. The trait exists so the seam (`orrery_sim_host`) can be handed a different *substrate* — an ECS world, per D42 (d) — while the canonical stage stays [`canonical_step`]…"* | **No.** Reads (d) as the record that admits an ECS-world substrate — which is what the restated clause says outright. |
| `crates/orrery_sim_host/src/lib.rs:274-280` | *"That is the whole of D42 (d)'s 'behind the seam': swapping this changes storage, never bytes."* | **No.** Relies on the seam constraint, which the restatement keeps verbatim. |
| `crates/orrery_sim_host/tests/ecs_differential.rs:623-626` | *"…this compares the thing D42 (d) actually names — the host."* | **No.** The restated clause still names the host behind the seam. |
| `scripts/core-gates.sh:486-491` | *"Armed by a crate hosting canonical state in a `bevy_ecs::World`. D42 (d) admitted exactly one such host (#757, `orrery_sim_host`'s `EcsBackend`) under a direct owner sanction, ahead of this battery…"* | **No.** Already written against the admission record. |
| `scripts/core-gates.sh:1-13, :240-259` | Clause 1's reasoning and `BEVY_PERMITTED_CRATES` — cite D42 (a) and D43 (e)(1), not (d). | **No.** Outside the amendment's scope by construction. |
| `docs/spikes/ecs-native-game-code.md:503`, `docs/spikes/neighbour-access-options.md:443` | Historical references to #745 having *named* D42 (d) as owed. | **No.** Dated snapshots; the spikes' own amendment-naming discipline is untouched. |

**Summary:** no record, plan, or comment relies on (d) reading trigger-gated in
a way the restatement falsifies. Everything that reads (d) as *the record of
the admission* becomes more accurate; everything that reads it as *the
trigger-gated rule* was already overtaken by recorded decisions and is named
here rather than silently left.

---

## 4. The drafted amendment

Proposed replacement for `docs/adr/0042-canonical-simulation-architecture.md`
lines 338-425 — the whole of clause (d), including both recorded blockquotes,
which are folded into the body rather than deleted (their content survives in
the restatement; the removed text remains in git history and in this diff).
The amendment note's status line is drafted as it would read once accepted;
**until the owner accepts, it is not in force and the tree is unchanged.**

```diff
-### (d) A dedicated `bevy_ecs::World` is admitted only behind the host seam, on pre-registered triggers
-
-> **Amended 2026-08-31 (owner-authorised). The host is admitted, and no
-> trigger fired.** On 2026-08-30 the owner sanctioned `bevy_ecs` adoption
-> directly; on 2026-08-31 it landed as `orrery_sim_host`'s `EcsBackend`
-> (#757), at four-class F-4 parity, with `scripts/core-gates.sh` at exit 0.
-> This clause prescribed its own amendment mechanism — *"amendment is an owner
-> decision recorded against this clause, not a silent edit"* — and this is that
-> record.
->
-> **What the admission did not do.** It did not fire T1, T2 or T3, and it did
-> not discharge the precondition package below. Read the trigger list as the
-> conditions under which adoption would have been *automatic*; the owner
-> retains the separate power to admit a host directly, which is what happened.
->
-> **The package ledger, as of this amendment:**
->
-> | Item | State |
-> |---|---|
-> | the differential parity harness beyond goldens | **met** — all four A10 §4.1 classes, generalised across substrates |
-> | A5's component-policy registry | **partial** — declaration data plane live and now the single source (`classify_component` retired, #761); IV-7's engine-handle refusal and the persistd linkage remain open |
-> | A4's Tier-H gate bundle (T3) | **not met** — see the open question below |
-> | capacity-scale mirror-cost numbers | **not met, and partly overtaken** — the admitted backend does not mirror into an application world, so A3's two-world-hop question is not the one now in front of us |
->
-> **T3's status is narrowed, not retired.** It no longer gates admission,
-> because admission has happened. It remains binding on *canonical bytes
-> moving*: no golden may be regenerated on the ECS path, and no second host may
-> be admitted, until [D43] clause (e)'s battery is enforced.
->
-> > **Discharged 2026-08-31 (owner-authorised).** [D43] clause (e)'s battery is
-> > now enforced and demonstrated mutation-style in full — all five clauses, each
-> > with a named killing mutation — and that record's confinement is lifted
-> > accordingly. T3 is satisfied for this host. It stays binding on any *further*
-> > adoption: a second host still enters through clause (e)(1)'s review-required
-> > allowlist and inherits the whole battery, which is the mechanism rather than
-> > a formality.
-> >
-> > The package's other two open items are unchanged by this and are **not**
-> > discharged: A5's component-policy registry is nearer (the declaration data
-> > plane is now the single source, `classify_component` retired) but IV-7's
-> > engine-handle refusal and the persistd linkage remain open; the
-> > capacity-scale mirror-cost numbers remain unmet.
-
-A dedicated canonical `bevy_ecs::World` — same per-entity topology, different
-substrate behind the seam — is neither adopted nor foreclosed. It becomes a
-legal *future* host implementation, adoptable only when a pre-registered
-trigger fires:
-
-- **T1:** a shipped module genuinely needs per-component canonical storage
-  with independent per-component policies — i.e. `CoreState`-as-one-enum
-  measurably stops scaling.
-- **T2:** measured tick cost in a real host shows the `BTreeMap` store
-  dominating.
-- **T3:** A4's Tier-H gate bundle lands and is demonstrated — mutation-style
-  — at least as strong as the clauses it replaces; a weaker bundle fails the
-  programme's standing bar ("a weaker gate that passes is worse") and must
-  not ship.
-
-T3 is a necessary precondition for any canonical byte to move regardless of
-T1/T2, per the pilot precondition list both lanes carry (A3 §7.4): the gate
-bundle, the differential parity harness beyond goldens, capacity-scale
-mirror-cost numbers replacing the indicative P4/P-1 bounds, and A5's
-component-policy registry. The trigger *definitions* may be amended by the
-owner (second opinion §3 V5 marks them "owner may amend"); amendment is an
-owner decision recorded against this clause, not a silent edit.
-
-**Until a trigger fires, the gated crates stay Bevy-free and
-`scripts/core-gates.sh` clause 1 stays exactly as it is** — the Bevy-free
-scan over `GATED_CRATES=(orrery_core orrery_games orrery_conformance)`
-(`scripts/core-gates.sh:37,66-77`). Nothing replaces the gate because
-nothing weakens it. (R2's Tier V discovery clause *strengthens* membership
-detection and is compatible with this sentence; the conditional Tier H
-battery arms only with a trigger here.)
-
-> **Amended 2026-08-31 (owner-authorised): the paragraph above no longer names
-> the tree.** The owner's acceptance in [#793] removes `orrery_games` from the
-> Bevy-free scan — see the second 2026-08-31 amendment to clause (a) for the
-> decision and the manifest evidence, and `scripts/core-gates.sh` clause 1 for
-> the corrected reasoning. **The set the sentence protects is now
-> `orrery_core` and `orrery_conformance`, and for those two it stands
-> unweakened.** No trigger fired; this is the same owner power clause (d)
-> already reserved and already exercised once for #757, recorded against the
-> clause rather than edited in silently.
->
-> Everything else in the paragraph holds: the declared floor
-> (`DECLARED_GATED_CRATES`) still carries `orrery_games`, so role discovery,
-> VC-4, VC-6, VC-8, the async-runtime ban and the neighbour-read scan all
-> still bind it. The only clause that stops binding it is the Bevy one.
+### (d) A dedicated `bevy_ecs::World` behind the seam: automatic on a fired trigger, otherwise by recorded owner decision
+
+> **Amended 2026-09-01 (owner-authorised).** This clause is restated so its
+> operative text states the mechanism the tree ran, instead of stating a rule
+> its own amendment notes override. Both admissions below were already
+> recorded against this clause (#759, #805); what the restatement changes is
+> where the rule lives — in the body, single-sourced — not what is licensed.
+> Nothing new is admitted here, nothing is foreclosed that was open, and the
+> trigger list stands.
+
+A dedicated canonical `bevy_ecs::World` — same per-entity topology, different
+substrate behind the seam — is a legal host implementation. It enters the
+tree one of two ways, and no third:
+
+**1. A pre-registered trigger fires, and adoption is automatic.** The
+triggers are measurements, not judgements; the owner may amend their
+*definitions* (second opinion §3 V5 marks them "owner may amend"):
+
+- **T1:** a shipped module genuinely needs per-component canonical storage
+  with independent per-component policies — i.e. `CoreState`-as-one-enum
+  measurably stops scaling.
+- **T2:** measured tick cost in a real host shows the `BTreeMap` store
+  dominating.
+- **T3:** A4's Tier-H gate bundle lands and is demonstrated — mutation-style
+  — at least as strong as the clauses it replaces; a weaker bundle fails the
+  programme's standing bar ("a weaker gate that passes is worse") and must
+  not ship.
+
+**2. The owner admits directly, by a decision recorded against this clause.**
+The trigger list is the automatic path, not the exclusive one — it never
+bound the owner's power over this record, and an exercise of that power is an
+amendment recorded here, never a silent edit. Both paths have been walked,
+and both walks are part of this clause's rule:
+
+- **2026-08-30, the host (sanction in [#745]):** *"`bevy_ecs` is approved and
+  S7 may start. No D42 (d) trigger fired; the owner sanctioned entry
+  directly."* It landed 2026-08-31 as `orrery_sim_host`'s `EcsBackend`
+  (#757) — the host behind the seam — at four-class F-4 parity, with
+  `scripts/core-gates.sh` at exit 0.
+- **2026-08-31, the rules (acceptance in [#793], "OWNER ACCEPTANCE,
+  2026-08-31"):** *"I accept the amendments. Bevy ECS in `orrery_games` as
+  first class dependency. ECS as idiomatic storage for entities and ruleset
+  logic. Systems registration and tick driving works over `World`."* What
+  that acceptance changed is recorded in clause (a) — reach, not location —
+  and what it changed *here* is the Bevy-free floor below.
+
+**The precondition package rides the admission, not the trigger.** The
+package both A3 lanes carry (A3 §7.4) binds whatever substrate is admitted,
+however it was admitted. Its ledger, as recorded against this clause and
+current at this amendment:
+
+| Item | State |
+|---|---|
+| the differential parity harness beyond goldens | **met** — all four A10 §4.1 classes, generalised across substrates |
+| A5's component-policy registry | **partial** — declaration data plane live and now the single source (`classify_component` retired, #761); IV-7's engine-handle refusal and the persistd linkage remain open ([D45]) |
+| the Tier-H gate bundle (T3) | **met for the admitted host** — [D43] clause (e)'s battery is enforced and demonstrated mutation-style in full, five clauses each with a named killing mutation; discharged there on 2026-08-31 |
+| capacity-scale mirror-cost numbers | **not met as written, and partly overtaken** — the admitted backend does not mirror into an application world, so A3's two-world-hop question is not the one now in front of us; the source half now has capacity-scale numbers and a recorded "partly discharged" judgement ([docs/14 §12.6]), and the consumer half does not exist |
+
+T3 keeps a second force the table does not retire: **no canonical byte moves
+without it.** Its admission-gating function lapsed when admission happened by
+sanction; its enforcement function is what a *further* adoption passes
+through — a second host enters through [D43] (e)(1)'s review-required
+allowlist and inherits the whole battery, which is the mechanism rather than
+a formality.
+
+**The Bevy-free floor, narrowed as recorded.** `orrery_core` and
+`orrery_conformance` stay Bevy-free and `scripts/core-gates.sh` clause 1
+stays exactly as it is for them; proposals to put `bevy_ecs` in either are
+refused by this record, not argued case-by-case. `orrery_games` is the one
+recorded exception, and the exception is permission, not dependency: the
+gate carries it in `BEVY_PERMITTED_CRATES` (`scripts/core-gates.sh:259`)
+while the crate's manifest declares none. Everything else about it binds in
+full — it stays in `DECLARED_GATED_CRATES` and `DECLARED_RULES_CRATES`, so
+role discovery, VC-4, VC-6, VC-8, the async-runtime ban and clause 5's
+neighbour-read scan all still apply — and if it ever hosts canonical state
+in a `bevy_ecs::World`, section 6's escape check fails it by name until it
+joins `DECLARED_HOST_CRATES` and takes the whole Tier-H battery
+([D43] (e)(1): eligible, not admitted). (R2's Tier V discovery clause
+*strengthens* membership detection and is compatible with this paragraph.)
+
+Nothing here touches clause (c): the shared application world stays rejected
+outright, with no trigger, pilot, or reversal path leading to it — a
+dedicated world shares nothing with presentation, and that distinction is
+the entire reason this clause could be restated without reopening (c).
```

Provenance notes on the restatement, so the owner can check it against the
records in one pass:

- **Nothing is admitted that was not already recorded.** Path 2's two bullets
  quote #745's body and #793's acceptance verbatim (§2.1, §2.4); the
  ledger's Tier-H row restates the nested discharge already recorded at
  `:367-373`; the capacity row keeps the recorded "not met, and partly
  overtaken" and adds only the pointer to the landed
  [docs/14 §12.6](../14-capacity.md) judgement.
- **The audit comment's four points** (§2.2): the admission gate is closed by
  paths 1–2; the package ledger by the table (each item waived, met, or
  still owed — none silently); T3's status by "met for the admitted host"
  plus the no-canonical-byte paragraph; point 4 was closed by clause (a)'s
  2026-08-31 amendment and is not touched here.
- **The trigger definitions T1/T2/T3 are carried verbatim.** Only the
  *framing sentence* around them changes — from "adoptable only when" to
  "automatic when" — which is the reinterpretation the clause's own first
  amendment blockquote already made (`:348-351`).
- **New record references:** `[D45]`, `[docs/14 §12.6]` and `[#745]` would
  need adding to the ADR's link table alongside the existing `[#793]` row.

---

## 5. What the amendment does not do

- **Clause (a) is untouched — canonical truth still never lives in an
  application world.** The load-bearing sentence and both of its 2026-08-31
  amendments stand exactly as written (`:156-223`); the restatement adds no
  store, no producer of canonical bytes, and no reach. `orrery_core` stays
  Bevy-free with its eleven-consumer justification intact, and the
  single-producer obligation on ruleset authors ("every committed byte is
  produced by `orrery_core::canonical_step`") remains what (a)'s own second
  amendment made it: an obligation enforced by [D43] clause (e) and
  `scripts/core-gates.sh`, not a waiver.
- **Clause (c) is untouched and was never amended.** The shared Bevy
  application world stays rejected outright — no trigger, no pilot, no
  reversal condition leads there (`:319-336`). The restatement's only
  mention of (c) is the closing sentence that says so.
- **Clause (e) is untouched.** The hybrid tier's pivot path and the binding
  of the pilot preconditions on its ECS-hosted tier are unchanged.
- **No other ADR is touched.** In particular D43 is not rewritten here: its
  clause (e) heading and honest-accounting paragraph carry the same
  body-vs-blockquote layering this amendment fixes for (d), and a companion
  D43 restatement is the natural follow-up — **recommended, not drafted.**
- **The trigger list is not retired.** T1/T2 are carried verbatim as the
  automatic path; T3's force on canonical bytes is restated, not weakened.
  A fired trigger still means adoption without a further owner decision.
- **Nothing operational changes.** No second host is admitted; no golden may
  move that could not move before; `orrery_games` still takes no Bevy
  dependency (permission is not dependency); the gate's scans, allowlists and
  escape checks are byte-identical. `./scripts/lane-diff-audit.sh` passes and
  no code file is in the diff.
- **The scope narrows to clause (d) only.** Three consequential items are
  named for the owner and deliberately not drafted: the ADR title (`:1`,
  still says "trigger-gated"); the
  [DECISIONS.md](../DECISIONS.md) index row (`:57`, same); and the
  consequences bullet's decorative "Until an R1 trigger fires" opening
  (`:456-458`). Each is a one-line edit the owner may take with the
  amendment or refuse without disturbing it.

---

## 6. #745's second item — related, separate, and left standing

Item 2 of #745, as recorded 2026-08-30:

> **The precondition package is incomplete.** A18 section 5 lists it
> independently of the triggers: Tier H demonstrated, A5's capability
> registry live, the four-class differential harness live, capacity-scale
> mirror numbers. **Tier H is empty** by D43 clause (e)'s own accounting, and
> D42 (d) named T3 — Tier H landed and demonstrated at least as strong as
> what it replaces — as necessary *regardless* of T1/T2.

**Current standing, verified:** the item's headline claim is overtaken by
events rather than discharged by work that followed its recording. Tier H
landed (#771; the battery is `scripts/core-gates.sh` section 6, five clauses
enforced with named killing mutations, `DECLARED_HOST_CRATES=(orrery_sim_host)`),
and T3's necessity-regardless-of-T1/T2 was satisfied for the admitted host by
the discharge recorded inside clause (d) itself. What remains of the package
is exactly what the recorded ledger says remains: the component-policy
registry's open enforcement (IV-7's engine-handle refusal, the persistd
linkage — owned by [D45](../adr/0045-per-component-capability-policy.md)) and
the capacity-scale numbers' consumer half (owned by
[docs/14 §12.6](../14-capacity.md)'s "partly discharged" judgement).

**Should the amendment address it? Recommendation, not decision: no — with
one narrow exception the draft already takes.** The exception: a *restated*
clause whose ledger still read "Tier-H gate bundle: not met" would be
internally false, so the restatement carries the ledger at current standing.
That is recording, not resolving. Beyond that, the amendment is the wrong
instrument for item 2's residue: what remains is work and evidence owed under
records that already own it (D45, docs/14), not record-text that misstates
anything. Leaving it standing as recorded risk is therefore both cheaper and
more honest than an amendment reaching into records it does not govern.

One adjacent recommendation for the accepting edit: A18 §5's S7 block still
asserts *"None has fired; Tier H is empty"* — false in fact since #757 and
#771 — and A22's track E still says *"only on a fired D42 (d) trigger"*. The
#745 audit comment already said these plan docs *"will read stale afterwards
and should be named in the decision."* Refreshing them is a documentation
chore (no lane), best done in the same edit that accepts this amendment or
immediately after — recommended, owner's call.

---

## 7. Verification

- `./scripts/lane-diff-audit.sh` — run on this lane's diff; passes.
- `check.sh` — exempt: documentation-only (AGENTS.md, "The push is the
  gate": prose and ADR work need no lane).
- The proposed diff touches `docs/adr/0042-canonical-simulation-architecture.md`
  and nothing else; no code, no gate, no golden.
- This document is a draft for the owner. **Nothing has been amended**: the
  ADR on this branch is byte-identical to `origin/main`, and the diff in §4
  is text, not a change.
