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
