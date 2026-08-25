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

## Decision

### (a) Only prediction rolls back — the per-tier law

Normative, per tier:

1. **Canonical state is correction-only.** No component of this system may
   rewind an authority's canonical entity state. Corrections are derived by
   isolated replay of the signed input log ([D9], [D10]) and applied forward
   as authoritative overwrites at the current tick. The authority's log is
   straight-line: late remote inputs are applied and logged at their arrival
   tick, never back-dated (docs/06:521, promoted here from documentation to
   decision).
2. **Durable state is recovery-only.** The durable tier reconstructs the
   latest state (checkpoint base + journal tail replay); it never rewinds to
   an older state and never serves one to a live client
   (`checkpoint/mod.rs:1-11`; docs/08 §1).
3. **Critical (P2) state is compensation-only.** Rows with economic value
   are corrected by compensating FDB transactions inside the intent
   envelope, never by rewind or overwrite outside it (docs/08 §2.2; A5
   IV-3/IV-5).
4. **Predictive resimulation is the only rewind**, and it is
   presentation-tier: it exists on the predicting side, for the local
   predicted set, and its output never feeds canon (docs/05:52; clause (d)).

A proposal that rewinds any of tiers 1–3 is refused by this record, not
argued case-by-case.

### (b) The unit

**The rollback unit is the per-entity predicted set.**

- **Grain:** `(entity × its R1 components)`. Within a rolled-back entity,
  R1 components restore from the ring; R0 components — ledger-backed P2 rows
  and cosmetic state — are untouched. The R dimension's meaning is D45's;
  this record fixes only which grain rolls back, subject to clause (e)'s
  restore rule.
- **Scope:** the local predicted set, entity by entity — never the world,
  never an island, never a cell (clause (c)). Each predicted entity
  reconciles against *its* authority's claims independently. Replay
  isolation is what makes this correct rather than merely cheap: steps read
  neighbours only from snapshots and cross-entity effects travel as
  next-tick events (`crates/orrery_core/src/executor.rs:106-142`), so
  re-stepping one entity never requires re-stepping its neighbours — the
  same property that makes single-entity adjudication valid makes per-entity
  rollback valid.
- **Window:** the existing 9-tick (150 ms) window over the 16-tick ring
  (docs/05:17, :68). This record adds no window.
- **Budget:** the existing [D8] degradation ladder
  Immediate → Amortize → Evict → SnapOwnPlayer (`budget.rs:191-265`,
  liveness M2). Any unit larger than the predicted set re-imports the cost
  the ladder exists to cap.

**The unit does not change under a triggered ECS host.** If a [D42] trigger
ever admits a dedicated world, its rollback substrate is a per-entity ring
of R1 components keyed by `PersistId` — the shape the presentation ring
already has — never a world snapshot, archetype clone, or copy-on-write
world fork, because those are mechanisms for the unit clause (c) rejects
(A7 §4.4). The unit binds the executor store today and any world-hosted
store later; it names no engine type.

### (c) World, island, and cell — rejected as units

- **Entire simulation world — rejected.** The tree rejects it in print:
  cost must scale with interest size, not world size — "the exact failure of
  whole-world rollback" (docs/05:66). Under [D42] there is no world-shaped
  canonical store to snapshot: canonical state is a per-entity
  `BTreeMap<PersistId, CoreState>` in the executor
  (`executor.rs:48-51`). And under per-entity authority ([D7]) there is no
  single timeline to rewind *to* — entity A's authoritative update at tick T
  says nothing about entity B's. The migration brief's worry about cloning a
  complete `World` is answered structurally, not by benchmark: the design
  has no consumer for a world snapshot.
- **Authority island — rejected.** No island-wide history exists anywhere;
  residuals and corrections are keyed by authority and entity —
  `TrackKey { authority: NodeId, entity: PersistId }`
  (`crates/orrery_predict/src/monitor.rs:47-52`). An island unit would make
  one entity's mispredict force resimulation of every island co-member —
  cost scaling with island population, the interest-vs-world failure one
  level down. Islands are an *authority* scope, not a *history* scope.
- **Spatial cell — rejected.** Cells are storage and interest keys, and an
  entity's committed cell changes mid-window: committed rekeys move an
  entity and its rows between cells (docs/08:60-63; the v2 `EntityRekey`
  record, `crates/orrery_protocol/src/persist.rs:279-304`). A unit that
  rekeys while the window is open is not a unit. Nothing simulates per cell.

### (d) L-1 — lightyear history is presentation-tier, and corrections use one door

The lightyear prediction history (its ring, its rollback machinery) is
**presentation-tier state**: never hashed, never persisted, never consulted
by canonical rules, and its contents appear in no encoded artifact.
Authoritative corrections cross into the presentation tier in exactly one
direction through exactly one door: `AuthorityCorrectionInbox`
(`crates/orrery_predict/src/correction.rs:48`), carrying `AdjudicatedState`
derived from hash-verified replay
(`crates/orrery_persistd/src/adjudication.rs:283-297`, `:573`; the
snapshot-hash-before-load gate is live — M3 kills
`a_snapshot_that_does_not_match_its_claim_is_forgery_not_deviation` by
name). Orrery owns membership, bounds, attribution, and evidence; lightyear
supplies mechanics only — it has no per-entity authority and no rollback
signal in 0.29 (`crates/orrery_predict/src/wiring.rs:36-56`). If lightyear
is ever replaced, this clause and clause (b) are unchanged: they name no
lightyear type.

### (e) Restore is all-or-nothing at the entity; partial restore is a named future door, not an open question

Component subset survives as **grain only** (clause (b)): which components
*participate* in rollback is per-component. **Restore is all-or-nothing at
the entity** for witnessed entities, decided, for a reason verified against
the shipped projection rather than assumed:

- The claim commitment is `state_hash = blake3(CoreCodec(quantized state))`
  over the entity's **whole** `CoreState`
  (`crates/orrery_core/src/ruleset.rs:319-326`;
  `executor.rs:125-127`, ordering pinned per Context §3). No claim commits
  to a component subset.
- The ring already honours this: for verifiable-core entities it snapshots
  the quantized core state and RNG cursor **as one block** (docs/05:60-64).
- Therefore a partially restored core state — some components from ring
  tick T, the rest left at the present — encodes to canonical bytes whose
  hash matches **no claim any authority ever made**. Every honest
  re-execution against it becomes a false deviation, the exact failure the
  deterministic-encoding invariant exists to prevent (A5 IV-2: "the witness
  convicts everyone, which is worse than watching no one").

**The owner's disposition, recorded verbatim:** partial restore *"may be a
future optimization on large coresets, but right now we accept all or
nothing."*

So this clause is a decided position with a named door, not a permanent
prohibition. For partial restore to become admissible, at minimum the
canonical witness projection would have to be able to commit to a component
subset without claiming a whole-state hash nobody produced — a
subset-addressable commitment is a **projection property, and the projection
format is R7's to define**, not this record's. Whoever opens the door owes,
against whatever projection then stands: a subset commitment the authority
actually signs, and an argument that mixed-tick states cannot reach the
comparators as false deviation. This record designs none of that; it names
the condition and leaves it.

## Consequences

- **What this record actually adds is smaller than its title** — the same
  admission [D42] and [D43] open their consequences with, and truer here.
  Clause (a) ratifies how all three mechanisms already behave; clause (b)
  ratifies the shipped ring, window, and ladder; clause (c) rejects units
  nothing implements; clause (d) ratifies the boundary the code already
  keeps. The record's value is that the migration cannot drift away from any
  of it: a pilot substrate proposal, a "roll back the island" incident
  response, or a lightyear replacement now has a record to answer to instead
  of prose to reinterpret. The genuinely new commitments are the per-tier
  refusals of clause (a) and the partial-restore disposition of clause (e).
- Corrections being forward-only overwrites means a client can transiently
  render state the authority has since corrected; that is [D8]'s
  reconciliation working as specified, and clause (d)'s one-door rule is
  what keeps the correction path auditable.
- Clause (e) accepts a real cost on large coresets: one mispredicted
  component restores the whole entity block. The owner priced this and
  accepted it; the door is named.
- **One caveat, marked unevidenced:** clause (b)'s window and budget bound
  resimulation *time* (M2 proves the ladder), but no in-tree measurement
  exists of the 16-tick ring's per-entity *memory* at capacity-scale
  predicted sets (A7 §12.5 recorded the same gap). If memory ever binds, the
  eviction rung is the existing lever; this record claims nothing about ring
  memory at scale.

## Alternatives considered

Alternative *units* — world, island, cell — are rejected in clause (c) with
their arguments. Alternative *mechanisms*, compared in A7 §4 and rejected
there, are summarized because each is rollback-shaped:

- **Archetype snapshots / copy-on-write world forks** (the ECS-pilot
  candidates). World-grain mechanisms for the unit clause (c) rejects; an
  archetype clone additionally bakes allocation-order-dependent layout into
  restored bytes, which the no-engine-artifact rule forbids from any encoded
  artifact (A5 IV-7; the projection-side enforcement is R7's).
- **The component journal as rollback substrate** (rewind by reading the
  bulk journal backward). Rejected for now on two mechanical grounds
  recorded by A7: the journal's `tick` field carries a client-local uplink
  sequence, not the universe tick (A7 F-1 — its disposition is the owner's,
  flagged, not this record's), and its payloads carry no schema statement
  (A5 G-3). Even with both closed, the ring serves the predicted-set unit at
  strictly lower cost, and canon corrects by replay of the cheaper, signed
  *input* log (A7 §4.3).
- **Domain-event journals as canonical truth.** Events are derived, not
  primary; canon is state commitments plus input logs, and an event journal
  would be a second, unverified account of the same history (A7 §4.5).
  Event-trace *fixtures* as test instruments are A10 programme material, not
  this record's.
- **lightyear's history as the canonical rollback substrate.** Refused by
  clause (d): it is presentation-tier, unhashed, unpersisted, and lightyear
  0.29 has neither per-entity authority nor a rollback signal to build on
  (`wiring.rs:36-56`).

## Verification appendix — what was re-run at acceptance

Every file citation above was opened on this tree at acceptance time; drift
found against A7's text: none that survives into this record (A7's own §11
stale-citation table was checked; its F-1 doc-vs-behaviour divergence is
cited as A7 recorded it and remains open). `docs/05:66`, `docs/05:52`,
`docs/06:521`, `ruleset.rs:319-326`, `executor.rs:47-51` and `:104-127`,
`monitor.rs:47-52`, `correction.rs:48`, `adjudication.rs:283-297`/`:573`,
`replay.rs:106-130`, `checkpoint/mod.rs:1-11`, `quantize_pin.rs:130`,
`p4-ledger.sh:409-414` all verified verbatim.

Mutations run at acceptance (break the stage → named check dies → revert →
passes; baselines recorded first; every failing run produced a real result
line):

| # | Guarded stage broken | Named check | Observed | Reverted |
|---|---|---|---|---|
| M1 | `step_entity` hashes **before** quantizing — A7 X-C's exact surviving swap, re-applied (`executor.rs:125-127`) | `cargo test -p orrery_conformance --test quantize_pin` | `the_claimed_hash_is_of_the_quantized_state_not_the_raw_one` FAILED; `1 passed; 1 failed` — the X-C gap is closed | `2 passed; 0 failed` |
| M2 | `RollbackBudget::plan`'s ladder collapsed — every over-frame replay amortized, eviction and floor unreachable (`budget.rs`, amortize guard forced true) | `cargo test -p orrery_predict --lib` | `budget::tests::overlong_replay_evicts_enough_to_fit` and `budget::tests::pathological_cost_snaps_the_own_player` FAILED; `41 passed; 2 failed` | `43 passed; 0 failed` |
| M3 | `load_claimed_snapshot`'s hash check removed — a snapshot loads unverified (`replay.rs:121-123`) | `cargo test -p orrery_core --test adjudication` | `a_snapshot_that_does_not_match_its_claim_is_forgery_not_deviation` FAILED; `14 passed; 1 failed` | `15 passed; 0 failed` |

All three sources restored byte-identical (`git status` clean); this record
is the branch's only change.

[D7]: 0007-authority-and-leases.md
[D8]: 0008-prediction-rollback-interpolation.md
[D9]: 0009-verifiable-core.md
[D10]: 0010-witnessing.md
[D13]: 0013-physics-and-determinism.md
[D38]: 0038-at-rest-schema-versioning.md
[D42]: 0042-canonical-simulation-architecture.md
[D43]: 0043-determinism-envelope-and-gate-replacement.md
[docs/05]: ../05-prediction-rollback.md
[docs/06]: ../06-verifiable-core.md
[docs/08]: ../08-persistence.md
[#426]: https://github.com/baadc0de/orrery/pull/426
