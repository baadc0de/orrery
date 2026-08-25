# ADR-0047: The rollback unit is the per-entity predicted set, and only prediction rolls back

**Status:** Accepted · **Date:** 2026-08-25 · **Decision:** D47

This record is normative. See the [ADR index](../DECISIONS.md) for precedence,
scope, and the complete accepted decision set.

Accepted by the owner on 2026-08-25 through the #395 planning tree (proposal
R6, [a11-adrs-and-pr-plan.md](../plans/a11-adrs-and-pr-plan.md) §2), with the
partial-restore disposition fixed in the owner's words as recorded in
clause (e).

**Supersedes:** nothing. It annexes and promotes into a decision record —
adjacent to [D8], which specifies the mechanics — prose that until now bound
only as documentation: [docs/05]'s predicted-subset-never-the-world rule
(docs/05:66) and [docs/06]'s straight-line-authority-log rule (docs/06:521).
It amends no accepted record's normative text; within the #395 proposal set,
R7 is the only proposal that does (it amends [D38]'s version-domain law), and
this record deliberately is not the second. It sits under [D42]'s canonical
simulation architecture and cites it rather than restating it. Its substance
is [a7-persistence-rollback-witnessing.md](../plans/a7-persistence-rollback-witnessing.md)
§2–§3 (proposals R-1 and L-1), carried into a record; every citation this
record leans on was re-opened and re-verified at acceptance time, and every
liveness claim was re-proven by mutation (Verification appendix).

Out of scope, each with its owner: the canonical witness projection format —
the entity-tick commitment unit, hashed-enumeration ordering, slot framing,
and the `projection_version` axis (R7, A7 §5; where this record needs a
projection property, clause (e) names it as R7's to define, not this
record's); per-component capability dimensions — this record *consumes* the
R dimension's meaning, R1 = "restores from prediction history", and does not
define it (D45); identity and the `PersistId` ↔ ECS entity mapping (D44);
command/event semantics — replay, dedup, idempotency (R5/D46); compatibility
manifests and digest storage (R8, A8/#404); the determinism envelope and
stage model ([D43]); persistence strategy — A7 P-1's finding that the
migration introduces no new strategy is inventory, not this record's clause,
and the four-tier hybrid stands as documented in [docs/08]. [D42] is the
umbrella. Nothing here schedules work inside the P4 digest before P4 exit:
the pipeline digest covers `crates/orrery_witness`, `crates/orrery_core`,
`crates/orrery_games` and `gates/p1-swarm` (`scripts/p4-ledger.sh:409-414`,
verified on this tree), and this record orders no code change at all — its
one enforcement gap was closed before acceptance by
[#426] (`bfefc5a6`, a conformance-crate test; Context §3).

## Context

### 1. Three rollback-shaped mechanisms exist, and only one of them rewinds

The question "what is the rollback unit?" presumes one rollback. The tree
operates three mechanisms that share only the word, and naming which of them
are *not* rollback is most of the answer. This is the strongest thing A7's
inventory produced (A7 §2), and this record leads with it:

| Mechanism | Tier | What rewinds | Evidence |
|---|---|---|---|
| **Predictive resimulation** | Presentation (client) | The mispredicting entity's predicted components, restored from a per-entity ring at the authoritative tick, then re-stepped | docs/05:60-68; `crates/orrery_predict/src/budget.rs:191-265` (the ladder; liveness M2) |
| **Authoritative correction** | Canonical | **Nothing.** The corrected state is re-derived by isolated replay of the signed input log and applied *forward* as an authoritative overwrite | docs/06:521; `crates/orrery_core/src/replay.rs:106-130` (liveness M3) |
| **Durable recovery** | Storage | **Nothing observable.** Recovery reconstructs the latest durable state from checkpoint base + journal tail; it never serves an older state to a live client | `crates/orrery_persistd/src/checkpoint/mod.rs:1-11`; docs/08 §1–§2 |

The canonical tier's rule is already in print and is quoted here because
this record promotes it to normative: *"Authorities never roll back their
own authoritative core entities — the log is straight-line by construction.
Remote inputs arriving late are applied (and logged) at the tick they
arrive"* (docs/06:521; the same rule from the prediction side at
docs/05:52). The critical durable path (P2, economically valuable rows) goes
one step further: it is corrected by **compensating transactions** inside
the FDB intent envelope, never by rewind (docs/08 §2.2; A5 IV-5).

So canonical state is never rolled back anywhere in this system, durable
state is recovered, critical state is compensated — and "the rollback unit"
is a question about the *predictive* mechanism only, plus a binding on what
any future ECS-hosted substrate may build.

### 2. What the predictive mechanism already is — verified live

Per predicted entity, per fixed tick, a 16-tick component ring holds: every
component registered for prediction; that tick's input-buffer entries; and,
for verifiable-core entities, the deterministic RNG cursor and quantized
core state as one block, so a replayed window is bit-identical
(docs/05:60-64). The rollback window is 9 ticks (150 ms) at the 60 Hz fixed
tick (docs/05:68). Cosmetic state is never snapshotted and never rolled back
(docs/05:66, applying [D13]'s classification). Cost is capped by the [D8]
budget ladder Immediate → Amortize → Evict → SnapOwnPlayer
(`budget.rs:191-265`), re-proven live at acceptance: collapsing the ladder
so eviction and the floor are unreachable kills
`budget::tests::overlong_replay_evicts_enough_to_fit` and
`budget::tests::pathological_cost_snaps_the_own_player` by name
(Verification appendix, M2).

The world-scope rejection is likewise already in print: *"Snapshotting only
the predicted subset — never the world — is the point: this is what makes
cost scale with interest size instead of world size, the exact failure of
whole-world rollback"* (docs/05:66).

### 3. The one enforcement gap A7 found is closed

A7 recorded mutation X-C as a survivor: swapping hash-before-quantize in
`step_entity` passed every suite (21 result lines, zero failures), because
every in-tree `CoreState` already stores lattice integers, so VC-7's
executor snap was a no-op on every fixture and the quantize-before-hash
ordering was pinned by nothing (A7 §10 X-C). That gap has since been closed
by [#426] (`bfefc5a6`): an off-lattice conformance ruleset plus
`the_claimed_hash_is_of_the_quantized_state_not_the_raw_one`
(`crates/orrery_conformance/tests/quantize_pin.rs:130`). Re-proven at
acceptance: re-applying X-C's exact swap (`executor.rs:125-127`) now fails
that test by name (Verification appendix, M1). This matters to clause (e):
the all-or-nothing argument rests on the claim hash committing to the whole
quantized state, and that commitment is now mutation-pinned, not assumed.
