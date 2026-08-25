# ADR-0046: Message-class semantics — six classes on one channel, dedup below the seam, and a flagged emission cap

**Status:** Accepted · **Date:** 2026-08-25 · **Decision:** D46

This record is normative. See the [ADR index](../DECISIONS.md) for precedence,
scope, and the complete accepted decision set.

Accepted by the owner on 2026-08-25 through the #395 planning tree (proposal
R5, [a11-adrs-and-pr-plan.md](../plans/a11-adrs-and-pr-plan.md) §2), with the
C-2 overflow posture fixed as recorded in clause (e) and C-2's constants
proposed to — not fixed for — the owner in clause (e)(5).

**Supersedes:** nothing, and it amends no accepted record's normative text.
Within the #395 proposal set, R7 remains the only proposal that will amend an
accepted record; this one does not. It sits under [D42]'s canonical
simulation architecture and consumes [D43]'s stage model S0–S7 and its
ordering-and-delivery-timing rules as fixed constraints — A4 fixed *when*
messages move and explicitly deferred "replay behaviour,
deduplication/idempotency … and volume bounds" to this record's substance
(`docs/plans/a4-deterministic-execution.md` §3.5 note at `:241-243`, §8);
that boundary was drawn deliberately, and nothing here restates or reopens
stage timing. Its substance is
[a6-commands-events-transactions.md](../plans/a6-commands-events-transactions.md)
§2–§4 and §9, carried into a record; A6's ground-truth table (§1), sequences
(§6–§8) and mutation log (§10) are incorporated by reference and re-verified
below where this record leans on them.

Out of scope, each with its owner: the determinism envelope, canonical
stages, and gate replacement ([D43] — decided; not restated); identity
classes and the `PersistId` mapping ([D44], drafted concurrently);
per-component capabilities ([D45], drafted concurrently); the rollback unit
(R6) and the canonical witness projection **format** (R7) — clause (e)
*places a field inside* whatever format R7 fixes, exactly as [D43](f) placed
its overflow flag, and decides nothing about the format itself; manifests,
schedule-digest storage, and the constant registry where C-2's numbers will
live (R8); and all implementation scheduling — nothing here starts work in
the P4 digest trees (`crates/orrery_witness`, `crates/orrery_core`,
`crates/orrery_games`, `gates/p1-swarm` — `scripts/p4-ledger.sh:409-414`,
re-verified verbatim) before P4 exit.

## Context

### 1. The channel discipline already ships; its semantics were undefined

Cross-entity effects travel one way today: a step emits events in emission
order ("Emission order is part of determinism — a `Vec`, never a set",
`crates/orrery_core/src/ruleset.rs:196`), the game's `deliver` maps each
event to its target's next-tick input, and the reference harness buffers them
per target (`crates/orrery_games/src/scenario.rs:206`, a
`BTreeMap<PersistId, Vec<CoreInput>>`) for application at the next tick.
Same-tick cross-entity visibility is structurally impossible: `step_entity`
removes the entity's own state before stepping so "a step cannot observe
another entity's mutation from the same tick"
(`crates/orrery_core/src/executor.rs:103-106`), and live neighbour reads are
gate-banned in rules crates with the recorded rationale that at replay every
neighbour read returns `None`, so "a rule that branched on one adjudicates
differently than it executed" (`scripts/core-gates.sh:126-132`).

What was **not** defined anywhere: how many kinds of message exist, which of
them may be deduplicated and by what key, what "immediate" is allowed to
mean, whether the input-composition order inside one tick is law or accident,
and what bounds a runaway emitter. A6 answered all five; this record fixes
the answers.

### 2. The composition order is convention by its own admission

The reference loop composes each entity's tick-input vector delivered-events
first, then player orders, under a comment that says so and disclaims it in
the same breath: "Events delivered from the previous tick come first, then
what an initial scenario player asked for. … The order is arbitrary but
fixed, which is all VC-2 requires."
(`crates/orrery_games/src/scenario.rs:210-214`, quoted verbatim from this
tree). "Arbitrary but fixed" is exactly the status this record ends: the
order stops being arbitrary the moment any shipped ruleset's outcomes depend
on it, and one already does — the mutation probe in the Verification appendix
(M-R5-1) shows the committed `skirmish/island` golden chain diverges when the
composition is reversed, while every other suite stays green.

### 3. Emission volume is unbounded, and determinism makes that invisible

The external seam has admission caps — `MAX_OPS_PER_INTENT` is 64
(`crates/orrery_persistd/src/intent/mod.rs:189`), with per-op and per-intent
byte caps beside it. The internal side has none: no constant like
`MAX_EVENTS_PER_STEP` exists anywhere in `crates/` (searched this tree), and
`StepOutput.events` is an unbounded `Vec`. A runaway rule can emit without
limit, and because authority and every honest replayer grow *identically*,
no hash ever disagrees — A6 §9's finding stands re-verified: deterministic
runaway is still runaway, and nothing detects it. Today this is theoretical
(Regolith emits single digits per tick even in volley scenarios, per A6 §9);
the clause below is normative-forward, not a ratification.

### 4. Steps cannot fail, and that is a property worth keeping on purpose

`Executor::step_entity` returns `Option<TickOutcome>`, and `None` means
exactly one thing: "an entity this executor does not hold"
(`crates/orrery_core/src/executor.rs:108-115`). There is no error variant, no
canonical failure path, no way for a rule step to abort a tick. A6 §12.2
posed C-2's posture question as fail-loud versus flag precisely because
fail-loud would *create* this tree's first canonical error path. [D43](f)
faced the same shape of question for arithmetic overflow and the owner chose
the flag there; the owner has now made the same choice here, and clause
(e)(3) names what that preserves.

## Decision

### (a) Six message classes, and the sixth is mechanically collapsed onto the third

The canonical taxonomy is six classes — **external commands**, **internal
deterministic commands**, **domain events**, **persistence events**,
**presentation events**, and **diagnostics** — with the definitions,
per-class rules (ordering, buffering, replay, rollback, idempotency, cyclic
dependency) and tier boundaries of A6 §2–§3 adopted as normative. Two points
of that adoption are decisions in their own right and are stated here rather
than left in the plan:

1. **Internal deterministic commands have no mechanism of their own.** An
   internal command *is* a domain event whose source module declares it a
   request rather than a report; it routes through the game's `deliver` into
   the target's next-tick input vector like every other event, and
   downstream — ordering, buffering, replay, rollback, idempotency — the two
   classes are indistinguishable. **Declining to add a channel is the
   decision.** An immediate internal-command channel is an observer cascade
   with a different name — A4's threat T7 ("immediate observer/hook cascades
   whose recursion depth and interleaving are unspecified",
   `docs/plans/a4-deterministic-execution.md:145`), which [D43]'s clause (c)
   already bans from the canonical path — and a *deferred* one would need its
   own log, its own ordering discipline, and its own replay reconstruction:
   three new artifacts to keep bit-identical across authority, witness and
   adjudicator, for a distinction nothing consumes. Events are never stored;
   they are re-derived from logged inputs at replay, so a second channel is a
   second replay story. The cost of the collapse is stated honestly: every
   cross-entity effect pays one tick of latency. That price is already being
   paid everywhere in the tree, and it is what isolated single-entity replay
   is made of.
2. **Class membership is a tier statement, not a naming statement.**
   Persistence events may not feed their producing tick; presentation feeds
   nothing canonical; diagnostics affect nothing and may be read by no rule.
   These generalize the own-state discipline from "rules don't read
   neighbours live" to "no class crosses a tier boundary backwards within a
   tick" (A6 §5, adopted).

### (b) C-1 — external dedup happens below the seam or by durable op-id, never by content

Deduplication of external commands is legal in exactly two places: transport
dedup **below the S0 seam**, before an input is sealed into the log; and
durable-tier dedup **by op-id reservation** at the intent gateway and FDB
transaction (the existing mechanism: gateway checks are a fast filter and
"the FDB transaction remains the sole authority",
`crates/orrery_persistd/src/intent/mod.rs:151-161`, with minted receipts so a
retried intent cannot double-mint). **Content matching inside the tick is
banned.** Once sealed, every log entry is real: two byte-identical commands
in one tick are two commands — Regolith's double-tap fires twice — and
content-based dedup would silently break every rule that legally repeats.
The same rule mirrored inward gives domain events their identity: an event
is *(emitter, emitting tick, emission index)*, by position and never by
value; identical payloads are distinct events.

### (c) Immediate versus stage-delimited — the audience rule

A6 §4's rules R1–R7 are adopted as normative, with [D43]'s stages supplying
the delimiters (this record adds no stage and re-times none). The governing
summary is restated because it is the part rules authors must carry:
**immediate is allowed only where the audience is the actor itself (own-state
writes within one step) or nobody canonical (diagnostics); every
cross-audience effect waits for a named stage boundary.** The design has
exactly three boundaries worth naming — next-tick delivery for events,
end-of-tick structural flush for spawns, post-tick drain for persistence and
corrections — and this record **declines to add a fourth**, because each
additional boundary multiplies the ordering surface replay must reproduce.
Clause (e) will lean on this: the emission-cap flag is written at an
*existing* boundary, not a new one.

### (d) Delivered-first input composition is ratified as law, and changing it costs a `RulesetId` bump

The composition of each entity's per-tick input vector — **inputs delivered
from the previous tick's events first, in producer order; then locally
submitted player orders** (`crates/orrery_games/src/scenario.rs:210-214`) —
is ratified from "arbitrary but fixed" convention into normative text. Any
conforming host composes in this order.

The consequence is stated because it is the real cost of the clause: **the
composition order is now part of the game's law, so changing it is a rules
change and costs a `RulesetId` version bump** (`RulesetId.version`, the
"game-assigned monotonic rules version",
`crates/orrery_protocol/src/verifiable.rs:59-63`) with regenerated goldens —
it is not a harness refactor. This is not hypothetical: mutation M-R5-1
(Verification appendix) reversed the composition and the committed
`skirmish/island` golden chain diverged, with the golden's own failure text
prescribing exactly that remedy ("If the rules changed on purpose, bump the
ruleset version and regenerate; if they did not, this is drift",
`crates/orrery_games/tests/battery.rs:239`). A game whose rules are
sensitive to delivered-versus-player ordering inherits this fixed convention
and cannot reorder without versioning — intended, because ordering is law
(VC-2), and A6 §12.3 put precisely this trade in front of the owner before
acceptance.

Enforcement is named honestly: the convention is pinned by **golden
state-level evidence** (`chains_match_the_committed_golden`), not by a unit
test on the composition function, and M-R5-1 shows only one committed
scenario is currently sensitive to it — the Regolith goldens pass under the
reversed order. The golden holds the clause; a composition-specific unit
check would pin it directly and is left as a suggestion, this being a
docs-only record.

### (e) C-2 — a per-entity emission cap whose overflow is a flag in witnessed state, not a failure

A6 §9 proposed the cap and offered two postures: fail the tick loudly
through a new canonical error path, or treat overflow as a
stage-1-style flag on an otherwise-completed tick. **The owner has chosen
the flag.** The clause, and the consequences that make the choice mean
something:

1. **The cap binds; flag posture is not unbounded emission.** A per-entity
   per-tick emission cap, kernel-checked at the close of S2 where the step's
   `StepOutput.events` is in hand: the first `MAX_EVENTS_PER_STEP` events in
   emission order are kept, and the suffix beyond the cap is **dropped,
   deterministically, and the drop is recorded** per sub-clause (2).
   Emission order is deterministic (`ruleset.rs:196`), so "the first N" is
   the same N on every honest host. A6's own warning is the thing this
   sub-clause defends against: "truncation is deterministic and therefore
   invisible" — *silent* truncation is the failure. Flagged truncation is
   truncation the evidence pipeline can see.
2. **The flag reaches witnessed state, or it proves nothing.** The reasoning
   is [D43](f)(3)'s, applied unchanged: a flag outside the hashed projection
   lets two hosts diverge — one flagged, one not — while `hash(e, t)` still
   matches, and the flag proves nothing precisely when it matters. Therefore
   the overflow record is a **per-entity discrete field of canonical
   state** — a saturating dropped-event counter (or occurrence bit;
   implementation's choice of width, not of location or distinctness) —
   written before S4, so that under R7's projection rules
   (`docs/plans/a7-persistence-rollback-witnessing.md` §5) it sits inside
   `bytes(e,t) = CoreCodec::encode(quantize(state(e,t)))` and hence inside
   `hash(e,t)` — the value a `StateClaim` commits to (WP-1). It is an
   integer, so S4 quantization is the identity on it and ring-2 comparison
   is exact (discrete axis; no band applies). By WP-3's one-sentence
   property, a flagged entity is flagged identically in the at-rest row, the
   replicated state, and every witness re-execution.

   **This is not [D43]'s arithmetic-overflow field, and the two must stay
   distinguishable.** [D43](f)(3) established a per-entity discrete field
   for *arithmetic* overflow, set by the rule's own operations during S2.
   Emission overflow is a different occurrence class with a different
   author: it is detected by the **kernel** at S2's closing edge, after the
   rule has returned, and it attributes a different defect (a rule emitting
   past its budget, not an arithmetic boundary). Collapsing both into one
   undifferentiated bit would make "why is this entity flagged"
   unadjudicable without re-execution archaeology. The two fields follow the
   same placement law (discrete, canonical, inside the claimed bytes) and
   may share a container word; they may not share a meaning. One
   consequence is acknowledged rather than hidden: the kernel writing a
   canonical state field at the close of S2 is a deliberate, narrow
   extension of A6 R4 ("own-state writes are the only immediate writes") —
   the cap is kernel law exactly as the intent admission caps are, and the
   write is an own-state write performed on the entity's behalf at its own
   step boundary, visible to no other entity that tick. No new stage
   boundary is created (clause (c)).
3. **Steps still cannot fail, and this clause is why they still cannot.**
   Choosing the flag preserves an existing structural property: there is no
   canonical error path today (Context §4), and this record **declines to
   create one**. That is a named benefit, not an accident — a fail-loud
   overflow would have handed any rule (or any attacker who can provoke a
   rule into emitting) a way to abort a tick, and [D43]'s alternatives
   record rejected the panic posture for arithmetic on the same ground: a
   loud failure in S2 takes the entity, and under a shared schedule the
   tick, down with it. The flag keeps the simulation running and the
   occurrence adjudicable. Arithmetic overflow and emission overflow are
   different questions — one is about what a value means at a type boundary,
   the other about volume — but the *flag-must-reach-witnessed-state*
   reasoning and the *no-canonical-abort* reasoning are genuinely common to
   both, and this record aligns with [D43] on exactly those two points and
   no further.
4. **Both honest parties flag identically on the same log, so adjudication
   is unaffected.** A6 argued this for fail-loud ("both honest parties fail
   identically on the same log"); it holds at least as well under the flag.
   The emission list is a pure function of `(own state, sealed inputs,
   TickRng)` — all committed before the step runs — so every honest
   re-execution produces the same events in the same order, truncates the
   same suffix, writes the same flag value, and computes the same hash. A
   *dishonest* authority that skips the cap diverges at its own emitter-side
   hash: not truncating means not setting the flag field, and the flag is
   inside the claimed bytes, so its claim disagrees with every honest
   re-execution of the same log and the ordinary deviation pipeline
   adjudicates it. Stated honestly rather than assumed: this catches the
   *emitter*. Whether an over-cap emitter's excess *deliveries* are
   independently caught at the target is a routing-verification question,
   and A6's mutation log shows routing correctness is currently
   golden-pinned only (M-A6-3) with the witness shown-ticks re-delivery
   immunity pinned by no named check at all (M-A6-4a) — this record relies
   on the emitter-side hash, which the flag placement makes sufficient, and
   does not claim target-side coverage that the evidence says is not there.
5. **The constants are the owner's, and they live in R8's registry.**
   `MAX_EVENTS_PER_STEP` **default 64**, mirroring `MAX_OPS_PER_INTENT = 64`
   (`crates/orrery_persistd/src/intent/mod.rs:189`, re-verified), is
   recorded as a **proposal the owner tightens or loosens** (a11 OD-28) —
   not a settled number; no shipped ruleset is within an order of magnitude
   of it. The constant is canonical law (changing it changes which logs
   flag, so it rides the same versioning discipline as any rules change) and
   its storage belongs to R8's manifest/registry work, as A6 §12 assigned.
   Two companions, adopted with the same status: a **delivery-queue bound**
   derived from the emission cap times the island's entity population —
   overflow of the *host's* queue is a host bug, asserts in debug builds,
   and is never canonical state; and the existing envelope bounds stand
   (diagnostics stay uncapped but sampled; persistence is already bounded at
   the FDB envelope, `intent/mod.rs:184-188`).

Nothing in sub-clauses (1)–(2) exists in code today; unlike clauses (a)–(d),
**C-2 is the genuinely new surface of this record** and is
normative-forward: implementation is post-P4 work, it widens canonical
state, and it lands under the same cost accounting [D43] recorded for its
flag (Consequences below).
