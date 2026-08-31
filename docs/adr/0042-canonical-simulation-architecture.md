# ADR-0042: Canonical state stays in the engine-neutral executor, the composition root and host seam land now, and a dedicated ECS world is trigger-gated

**Status:** Accepted · **Date:** 2026-08-25 · **Decision:** D42

This record is normative. See the [ADR index](../DECISIONS.md) for precedence,
scope, and the complete accepted decision set.

Accepted by the owner on 2026-08-25 through the #395 planning tree, as
proposal R1 of [A11](../plans/a11-adrs-and-pr-plan.md) §2 ([#407]).

**Supersedes:** nothing. It is the umbrella record of the #395 architecture
programme: it turns the position both A3 lanes reached independently
([a3-simulation-host-comparison.md] §7; [a3-simulation-host-second-opinion.md]
§6) — and that A4–A10 consumed as the architecture — into normative text, and
it absorbs [A9](../plans/a9-engine-boundaries.md) §2.1's boundary statement
B-1 ("canonical truth never lives in a Bevy application world") so that the
rule is stated once. It **extends** [D15]'s crate layering (the normative
spine restated as [docs/10 §2] layering rules 1–2) with the host-seam layer of
clause (b). It names no frozen surface and does **not** reopen [D21]: no
`Ruleset` trait change, no harness-API change, and no change to link-time
distribution is made or implied here. It amends no accepted record's normative
text.

Out of scope, decided by their own proposals or not at all: the determinism
envelope, canonical stage set, and any replacement of the core gates (R2);
identity classes and allocation (R3); per-component capability policy and
`classify_component`'s successor (R4); message-class semantics (R5); the
rollback unit (R6); the canonical witness projection format and version axes
(R7); compatibility manifests and the `RulesetId` digest (R8); every owner
decision OD-1..OD-34 of A11 §3; and all implementation scheduling — nothing
here starts work in the P4 digest trees (`crates/orrery_witness`,
`crates/orrery_core`, `crates/orrery_games`, `gates/p1-swarm` —
`scripts/p4-ledger.sh:409-414`) before P4 exit. The composition root and seam
are Tranches 3–4 of A11 §5.2, gated on that exit. This record is the umbrella
the other proposals assume; it decides none of their content.

## Context

### 1. The dedicated-store topology already ships

The question the migration brief posed — dedicated canonical world versus
shared application world — is not an open future choice on this tree. The
canonical, verifiable, witness-hashed state of every entity lives in the
engine-neutral per-entity executor:

```rust
pub struct Executor<R: Ruleset> {
    ruleset: R,
    seed: UniverseSeed,
    states: BTreeMap<PersistId, R::CoreState>,
}
```

(`crates/orrery_core/src/executor.rs:48-52`; `BTreeMap` rather than `HashMap`
because VC-4 makes iteration order observable). Every host today — the
regolith client, the p1-swarm bots, the witness engine's replay executor,
persistd's `AdjudicationExecutor` — keeps canonical state in an `Executor`
outside any Bevy world and mirrors what presentation needs; canonical state
crosses the wire as engine-neutral bytes; the backend runs the same store
with no engine at all (second opinion §2, evidence E-1/E-2/E-5). The
`BTreeMap` inside `Executor` *is* the dedicated canonical world, minus the
word "world".

Clause (a) therefore **ratifies what ships**. Its value is the change of
status: what has been an incidental property of the implementation — one a
refactor could have quietly traded away — becomes the decided topology, and
A9's B-1 becomes its boundary form.

### 2. Two independent comparisons, and what they did and did not agree on

Two A3 lanes ran the comparison separately, with different variant framings,
weights, and scoring scales:

- [a3-simulation-host-comparison.md] §6: V1 (improve in place) 456/500,
  H1 (hybrid) 378, V4 (bespoke) 372, V3 (dedicated ECS world) 356,
  V2 (shared application world) 268.
- [a3-simulation-host-second-opinion.md] §5: V5 (hybrid: composition root +
  host seam, ECS trigger-gated) 150/165, V1 146, V4 131, V3 104, V2 62.

They **agreed on the action**: keep the executor topology; build the
composition root and the `SimulationHost` seam now; reject the shared world
outright; admit a dedicated `bevy_ecs::World` only later, behind the seam, on
pre-registered conditions. That agreement, reached from independent evidence
passes, is this record's evidence base.

They **disagreed on the ordering of the top two variants** — the first lane
scored improve-in-place above its hybrid; the second scored the hybrid-with-
seam first — and the second lane said so plainly: "V5 (150) beats V1 (146) by
less than the resolution of this method. … The V5-over-V1 call is therefore
argued, not computed" (second opinion §5). The seam is adopted here on that
argument, not on the score: it is required by the three-driver divergence and
any Unreal attach point regardless of storage decisions, so its cost is not
stranded under any future — including "never migrate". This record does not
smooth that disagreement into unanimity; it adopts the shared action and
records that the tie-break was argued.

### 3. The strongest single fact against the shared world

The shared world's headline benefit — native integration with the
lightyear/replicon client stack — attaches to machinery whose central
capability does not function. The pinned registry crate's own documentation
states:

> Authority is currently not working since replicon only supports server to
> client replication.

(`lightyear_replication-0.29.0/src/lib.rs:67-68`, re-read in the registry
source for this record — a **registry** crate under `~/.cargo/registry`, not
vendored; `vendor/` holds only `aeronet_iroh`, `aeronet_tokio_runtime`, and
`bevy_replicon`.) The one thing Orrery most needs at the canonical level —
per-entity authority — is Orrery-side in full (`orrery_authority`, D7),
whether or not canonical components sit in the app world. A shared world
would buy integration with a mechanism that cannot carry the authority model
anyway.

The structural argument is heavier than the integration one, and it is the
reason for clause (c): under the shipped topology, witness projection
*cannot* include presentation state — the hash is computed in `orrery_core`
from canonical bytes of a `CoreState` defined in Bevy-free crates, and
presentation components are unreachable from that call site by construction;
rollback touches executor state only; the backend links zero Bevy (A9 §2.1's
consequence table). In a shared world every one of those guarantees degrades
from storage fact to review-held convention — schedules and an unbuilt policy
registry would have to *exclude* what the structure today makes
*unreachable*. The migration brief itself lists the coupling and
rollback-contamination risk of its shared variant
(`docs/plans/ruleset-ecs-migration-brief.md:468`).

### 4. What is measured, and what is only feared

Honest accounting, carried from the lanes' disputed-claims ledgers:

- **Measured:** the mirror cost the shared world would save is ~9 µs per 10k
  entities extracted (A3 P4, marked indicative) — ~1.5 % of a tick (second
  opinion P-1). The brief's "copying is slow" motivation is measured false at
  current scale.
- **Measured absent:** the brief's central fear, generic infection of the
  codebase by the trait model, was looked for and not found (second opinion
  §2, citing A1 §4.4).
- **Unevidenced, and marked so:** that composition behind one trait will hold
  modularity at second-game scale. Both lanes carry this as the open risk
  (first lane: "god-trait pressure unrefuted [U]"); it is exactly what the
  A10 E-1 experiment exists to test, and clause (e) pre-registers what
  happens if it fails. Neither lane's matrix let unevidenced claims move
  scores.

## Decision

### (a) Canonical verifiable state stays in the engine-neutral per-entity executor

The canonical store of record for verifiable state is the `Executor`'s
`BTreeMap<PersistId, R::CoreState>`
(`crates/orrery_core/src/executor.rs:48-52`), owned by whatever hosts the
simulation. This ratifies the shipped topology; it changes no code.

> **Amended 2026-08-31 (owner-authorised), against the admission recorded in
> clause (d).** The sentence above named one store because one store existed.
> Since #757 a second substrate satisfies the same contract: during an
> ECS-backend tick the store of record is that backend's `Canonical`
> component. **What is normative is not the container but the seam** — every
> committed byte is produced by `orrery_core::canonical_step`, which both
> backends call, and `orrery_core` carries no Bevy dependency. Read this
> paragraph as naming that seam. The clause's load-bearing sentence below —
> canonical truth never lives in a Bevy *application* world — is unchanged and
> unbreached: the admitted world is dedicated, reachable only through the
> backend, with no `bevy_app` and no `&World` accessor.

The boundary form is A9's B-1, absorbed here verbatim in substance:
**canonical truth never lives in a Bevy application world.** Application
worlds hold only *mirrors* — presentation and replication components keyed by
the `PersistId` component, written by mirror writes classified as
presentation events (A6/E-10). Any future host, on any engine, is a mirror
consumer of engine-neutral canonical bytes. B-1 and this clause are one rule;
no other record restates it.

What enforces the rule, and exactly how far each mechanism reaches, is A9
§2.2's table — dependency structure and the hash call site are structural,
`core-gates.sh`'s crate list is exactly its list, and the replicated-payload
corridor is review-held until R4/OD-26 close it. This clause adopts the rule;
it does not upgrade or replace any enforcement mechanism (that is R2's and
R4's business).

### (b) The composition root and the `SimulationHost` seam land now

Two build commitments, both variant-independent — the property both lanes
verified separately: they pay for themselves under every future this
programme contemplates, including "never migrate".

1. **Composition root** (brief phase 2): the game is assembled from named
   per-domain rule modules that each own a section of `CoreState` and a
   slice of the input/event vocabulary, delegating behind the **existing,
   unchanged** `Ruleset` contract; cross-module couplings get owners per A2
   §5.3 (visible, ordered, owned, event-composed). This adopts the brief's
   modularity motivation while rejecting its storage conclusion. The
   manifest struct's shape and the module construct are R8's.
2. **`SimulationHost` seam** (brief phase 3): one kernel-owned driver owning
   tick advance, stable-id lookup, command-in/event-out, and output
   collection — the loop all three of today's hosts hand-roll (A2 §7.5). The
   Bevy client, the future field host, and any Unreal sidecar drive the same
   host API. **The host's storage is an implementation detail behind the
   seam**, and today that storage is clause (a)'s executor: the seam moves
   no state.

Layering: the host seam extends [D15]'s spine as a layer between
`orrery_core` and the hosts that drive it. It adds a crate and an adapter; it
does not alter D15's two normative rules ([docs/10 §2] rules 1–2), and the
gated crates' dependency posture is unchanged by construction — see
clause (d).

Sequencing is not decided here: both items touch P4-digest territory and land
as A11 §5.2 Tranches 3–4, after P4 exit, behind their listed dependencies.

### (c) The shared Bevy application world is rejected outright — not deferred

Hosting canonical state in a world shared with presentation and replication
state is rejected as an architecture, permanently, on the record's evidence:
last in both independent matrices by unbridgeable margins (268/500, −188 from
leader; 62/165, last on every reweighting either lane tried), and rejected
for the *kind* of loss, not the score: it is the only variant that changes
the shipped topology, and it converts the system's structural guarantees —
witness projection that cannot reach presentation state, rollback scoped to
executor state, a zero-Bevy backend — into review-held convention (Context
§3). Its headline benefit attaches to an authority mechanism that is
non-functional by its own documentation
(`lightyear_replication-0.29.0/src/lib.rs:67-68`), and the mirror cost it
would save is measured at ~1.5 % of a tick.

Rejected outright means: no trigger, no pilot, and no reversal condition in
this record reopens it. Clause (e)'s reversal path explicitly does not lead
here.

### (d) A dedicated `bevy_ecs::World` is admitted only behind the host seam, on pre-registered triggers

> **Amended 2026-08-31 (owner-authorised). The host is admitted, and no
> trigger fired.** On 2026-08-30 the owner sanctioned `bevy_ecs` adoption
> directly; on 2026-08-31 it landed as `orrery_sim_host`'s `EcsBackend`
> (#757), at four-class F-4 parity, with `scripts/core-gates.sh` at exit 0.
> This clause prescribed its own amendment mechanism — *"amendment is an owner
> decision recorded against this clause, not a silent edit"* — and this is that
> record.
>
> **What the admission did not do.** It did not fire T1, T2 or T3, and it did
> not discharge the precondition package below. Read the trigger list as the
> conditions under which adoption would have been *automatic*; the owner
> retains the separate power to admit a host directly, which is what happened.
>
> **The package ledger, as of this amendment:**
>
> | Item | State |
> |---|---|
> | the differential parity harness beyond goldens | **met** — all four A10 §4.1 classes, generalised across substrates |
> | A5's component-policy registry | **partial** — declaration data plane live and now the single source (`classify_component` retired, #761); IV-7's engine-handle refusal and the persistd linkage remain open |
> | A4's Tier-H gate bundle (T3) | **not met** — see the open question below |
> | capacity-scale mirror-cost numbers | **not met, and partly overtaken** — the admitted backend does not mirror into an application world, so A3's two-world-hop question is not the one now in front of us |
>
> **T3's status is narrowed, not retired.** It no longer gates admission,
> because admission has happened. It remains binding on *canonical bytes
> moving*: no golden may be regenerated on the ECS path, and no second host may
> be admitted, until [D43] clause (e)'s battery is enforced.

A dedicated canonical `bevy_ecs::World` — same per-entity topology, different
substrate behind the seam — is neither adopted nor foreclosed. It becomes a
legal *future* host implementation, adoptable only when a pre-registered
trigger fires:

- **T1:** a shipped module genuinely needs per-component canonical storage
  with independent per-component policies — i.e. `CoreState`-as-one-enum
  measurably stops scaling.
- **T2:** measured tick cost in a real host shows the `BTreeMap` store
  dominating.
- **T3:** A4's Tier-H gate bundle lands and is demonstrated — mutation-style
  — at least as strong as the clauses it replaces; a weaker bundle fails the
  programme's standing bar ("a weaker gate that passes is worse") and must
  not ship.

T3 is a necessary precondition for any canonical byte to move regardless of
T1/T2, per the pilot precondition list both lanes carry (A3 §7.4): the gate
bundle, the differential parity harness beyond goldens, capacity-scale
mirror-cost numbers replacing the indicative P4/P-1 bounds, and A5's
component-policy registry. The trigger *definitions* may be amended by the
owner (second opinion §3 V5 marks them "owner may amend"); amendment is an
owner decision recorded against this clause, not a silent edit.

**Until a trigger fires, the gated crates stay Bevy-free and
`scripts/core-gates.sh` clause 1 stays exactly as it is** — the Bevy-free
scan over `GATED_CRATES=(orrery_core orrery_games orrery_conformance)`
(`scripts/core-gates.sh:37,66-77`). Nothing replaces the gate because
nothing weakens it. (R2's Tier V discovery clause *strengthens* membership
detection and is compatible with this sentence; the conditional Tier H
battery arms only with a trigger here.)

### (e) The pre-registered reversal condition

If an A10 E-1-class experiment — a second-game-scale module set built
against the composition root, instrumented for central-dispatch pressure —
shows that composition behind one trait cannot hold modularity at that
scale, then the composition-root claim of clause (b)(1) is dead and the
pivot is the **hybrid tier model** (A3 H1 / second-opinion V5's tiered
future): a verified core tier on the executor plus a bulk tier, with the
boundary explicit. The pivot is **not** the shared application world —
clause (c) survives this reversal — and the pilot preconditions of
clause (d) stay binding on the hybrid's ECS-hosted tier regardless.

## Consequences

- **What this record actually adds is smaller than its title.** Clause (a)
  ratifies the shipped topology and changes no code; clause (c) rejects a
  variant nothing implements. The record's real new commitments are
  clause (b) — building the composition root and the seam — and clause (d) —
  pre-registering what would justify going further. A reader weighing its
  cost should weigh those two.
- Accepting the topology as normative re-scopes R2–R8: each of them now
  assumes an engine-neutral per-entity canonical store and a host seam, and
  refusing any of them cannot silently reintroduce the shared world.
- The seam is churn that intersects the P4 digest, and a seam with one
  implementation risks being speculative structure. The mitigation is the
  variant-independence argument of clause (b) — it is the phase-3 step of
  every forward path the brief contemplates — plus the reversal condition of
  clause (e), which names the experiment that would kill the composition
  claim rather than leaving it unfalsifiable.
- Until an R1 trigger fires, proposals to put `bevy_ecs` in a gated crate
  are refused by this record, not argued case-by-case; and the trigger list
  means such a proposal now has a legitimate path — produce the T1/T2
  measurement and the T3 gate bundle — instead of a standing argument.
- The rejection in clause (c) forfeits, permanently, the shared world's real
  conveniences: zero mirror hop and idiomatic Bevy module shape (modularity
  and copy cost are the only axes where either matrix scored it well). This record
  judges them cheap — the hop is measured at ~1.5 % of a tick and modularity
  is bought structurally by clause (b) — and accepts the loss.
- The enforcement asymmetry A9 §2.2 documents is unchanged by this record:
  one of the fences (the replicated-payload corridor, A5 G-1) remains made of
  review until R4/OD-26 close it. Adopting the rule here does not close it,
  and saying otherwise would overstate the record.

## Alternatives considered

- **Shared Bevy application world** (brief Variant B; A3 V2). Rejected
  outright — clause (c) and Context §3. Last in both matrices; converts
  structural guarantees into convention; integrates with a non-functional
  authority mechanism; saves a measured-cheap mirror.
- **Adopt a dedicated `bevy_ecs::World` now** (A3 V3). Rejected as a
  present-tense decision, preserved as clause (d)'s triggered future. Every
  immediate cost is real — an allow-list gate weaker than clause 1 of
  `core-gates.sh`, new determinism hazard classes (second opinion P-2), Bevy
  version coupling near the kernel, P4 churn — and every immediate benefit
  lacks a consumer: no canonical rule queries components today, and the
  per-entity replay contract reduces a world to a fancier `BTreeMap` anyway
  (second opinion §2).
- **Bespoke generalized engine-neutral core, ECS forsworn** (A3 V4).
  Rejected for its foreclosure, not its architecture — its architecture *is*
  today's. Committing never to adopt ECS on one production game's evidence is
  the same over-decision as adopting it now, in the opposite direction; and
  generalizing the executor means rebuilding scheduling, queries, and tooling
  to gain a capability (structural multi-entity reads) that isolated
  single-entity replay cannot adjudicate.
- **Improve in place, without the seam** (A3 V1). The named fallback, beaten
  narrowly and only on argument (Context §2). If the owner had judged the
  seam speculative, V1 is correct and loses only the Unreal attach point and
  driver convergence. It was not chosen because the three-host driver
  divergence is real today and every forward path re-invents the seam later
  at retrofit prices.
- **Decide storage later, build nothing now.** Rejected by both lanes:
  it leaves the god-trait pressure unanswered, leaves three hand-rolled tick
  loops diverging, and leaves the topology an accident a refactor could
  trade away without a record to answer to.

[a3-simulation-host-comparison.md]: ../plans/a3-simulation-host-comparison.md
[a3-simulation-host-second-opinion.md]: ../plans/a3-simulation-host-second-opinion.md
[D15]: 0015-crate-set.md
[D21]: 0021-ruleset-distribution.md
[docs/10 §2]: ../10-crates.md
[#407]: https://github.com/baadc0de/orrery/issues/407
