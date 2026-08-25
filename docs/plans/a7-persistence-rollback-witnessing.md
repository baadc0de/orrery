# A7 — Persistence, rollback unit and canonical witness projection (#403)

**Status:** decision proposal for the #395 planning tree · **Date:** 2026-08-25 ·
**Tree:** `docs/403-a7` (based on `main` at `3195583d`) · **Parents:**
[#403](https://github.com/baadc0de/orrery/issues/403) ←
[#395](https://github.com/baadc0de/orrery/issues/395) · **Builds on:**
[A1](a1-ruleset-architecture-map.md), [A2](a2-kernel-game-module-ownership.md),
[A3](a3-simulation-host-comparison.md) (+ the preserved
[second opinion](a3-simulation-host-second-opinion.md)),
[A5](a5-identity-and-capabilities.md), and A4 (PR #418, in flight — cited as
PR content, not as `main`) · **Source brief:**
[docs/plans/ruleset-ecs-migration-brief.md](ruleset-ecs-migration-brief.md)
§Persistence, rollback, and witnessing

Three decisions were reserved to this node by name: the **rollback unit**
(A2 §7.1; A5 §8.1 — the R dimension "records membership only"), the
**canonical witness projection** (A4 §8; A5 §8.1), and the persistence
**strategy comparison** the brief demands (snapshots · component journals ·
domain-event journals · the existing transaction journal · hybrids). All
three are settled below — as proposals. Accepting or amending anything here
is the owner's (#395: propose, do not decide); ADR text belongs to A11 (#407).

Method, as in the predecessors:

- Every claim cites a file and line opened on this tree today. Where this
  document asserts a property is *enforced*, the **guarded stage** was broken
  (not the check line), the named check that died recorded with its real
  result line, the change reverted, the pass re-confirmed (§10). Two runs
  produced results against this document's own convenience and are reported
  as such: one mutation **survived** every suite it faced (§10 X-C), and one
  mutation was *designed* to survive the goldens and did — while killing six
  event-assertion tests the goldens story never mentions (§10 X-A).
- What **exists**, what is **designed but unwired**, and what is **proposed
  here** never share a sentence.
- Where a decision belongs to another node — command/event semantics (A6,
  #402, being written in a sibling lane right now), manifest format (A8,
  #404), test-programme construction (A10, #406) — it is named, not decided
  in passing (§9).

---

## 1. Ground truth inherited and re-verified

Each finding this document leans on was re-checked on this tree before use.

| # | Finding | Re-verification |
|---|---|---|
| I1 | Canonical rules state lives in `Executor`'s `BTreeMap<PersistId, R::CoreState>`; the map choice is VC-4-motivated | `crates/orrery_core/src/executor.rs:48-51`, comment at `:60-63` |
| I2 | The witness hash is per-entity: `state_hash = blake3(CoreCodec(quantized state))`; **no container is iterated into it** | `ruleset.rs:319-326` ("blake3 over the canonical encoding of the **quantized** state (VC-7), so a claim commits to exactly what replication and persistence saw"); `executor.rs:125-127` |
| I3 | Query iteration order over a `bevy_ecs` world is allocation/archetype-dependent; a sorted-by-stable-id projection agrees across orders. Reproduced **three times independently**: A3 P1/P2, second opinion P-2, A4 E-3 (`f6a3…` vs `d243…`) | Relied on as recorded (prototype evidence; no repo delta since A3's runs). Not re-run — the three independent reproductions are the point |
| I4 | Goldens are chains over per-tick state hashes **only**; the source says so itself and names the blind spot: "adding attribution to `Outcome::DamageDealt` did not shift a single chain" | `crates/orrery_games/src/golden.rs:20-29`. Re-proven live by mutation X-A (§10): an injected event-only outcome leaves all 11 battery tests green |
| I5 | `cargo tree -p orrery_witness \| grep -ci bevy` = **530** while `./scripts/core-gates.sh` exits **0**; `GATED_CRATES=(orrery_core orrery_games orrery_conformance)` is a typed list | Both halves re-run on this tree today: 530, exit 0. `scripts/core-gates.sh:37` |
| I6 | A3's adopted position (both lanes): canonical verifiable state stays in the engine-neutral per-entity executor; shared Bevy app world **rejected**; a dedicated `bevy_ecs::World` admitted only behind the host seam on named triggers (T1–T3) | a3-simulation-host-comparison.md §7; second opinion §3 V5 |
| I7 | A5's model: three closed identity classes; `bevy_ecs::Entity` may appear in no encoded artifact outliving its world (IV-7); any enumeration for hashing sorts by `PersistId` (N-2.3); five capability dimensions P/R/W/N/A with zeros failing closed; the R dimension records rollback membership and defers unit + mechanism here | a5-identity-and-capabilities.md §2, §5; IV rows re-read |
| I8 | A4 (PR #418): 14 nondeterminism entry paths → 13 mechanisms, each with a named check; canonical stage model S0–S7 with S4 (Quantize) before S5 (Claim) non-negotiable; Tier V role-discovery gate + conditional Tier H (arms only if an ECS host is admitted) carrying `ambiguity_detection=Error`, a projection differential harness (E-M3), and single-entity step exposure | PR #418 `docs/plans/a4-deterministic-execution.md` §2–§5, fetched and read as `pr418`; its two headline figures re-verified first-hand (I5) |
| I9 | The durable tier is **already a snapshot+journal hybrid**: "the checkpoint is the base, the journal is the delta, so recovery is zero-loss by construction"; checkpoints reach FDB on a 20 s jittered cadence; journal records are idempotent, "keyed by `(entity, tick)` with last-writer-wins per component within an entity's single-writer stream" | `crates/orrery_persistd/src/checkpoint/mod.rs:1-11`; docs/08-persistence.md §1 diagram + §2 table (`:79`); `orrery_protocol/src/persist.rs:200-205` |
| I10 | Authorities never rewind authoritative core entities — "the log is straight-line by construction"; late remote inputs are applied and logged at their arrival tick, never back-dated | docs/06-verifiable-core.md:521; docs/05-prediction-rollback.md:52 |
| I11 | Client-side rollback exists and is bounded: per predicted entity, per fixed tick, a 16-tick component ring; window 9 ticks (150 ms); budget guard ladder Immediate → Amortize → Evict → SnapOwnPlayer; cosmetic state never snapshotted, never rolled back; "Snapshotting only the predicted subset — never the world — is the point" | docs/05-prediction-rollback.md:17, :60-68; ladder live-proven by mutation X-E (§10): breaking the eviction rung kills two named tests |
| I12 | lightyear 0.29 supplies prediction mechanics only: no per-entity authority (its own doc: "Authority is currently not working…", quoted at `orrery_predict/src/wiring.rs:37-41`) and **no rollback signal** — the per-entity residual arrives as `VisualCorrection<D>` after `RollbackSystems::EndRollback` | `wiring.rs:36-56`; `predict/lib.rs:1-30` |
| I13 | Adjudication replays exactly one entity from a hash-verified snapshot; corrections flow back as `AdjudicatedState` through `AuthorityCorrectionInbox` | `replay.rs:106-130` (hash check before load — live-proven by mutation X-B); `adjudication.rs:283-297`, `:573`; `correction.rs:48` |
| I14 | The at-rest schema machinery is live and fail-closed: per-`(ComponentTypeId, SchemaVersion)` slots, envelope floor, undeclared component ⇒ refuse, future version ⇒ refuse, `EntityRekey` v2 refuses v1 | A5 §7 (mutation-proven X3/X4 there); the rekey refusal re-proven fresh here by mutation X-D (§10) |

### 1.1 New findings made while verifying (not in any predecessor)

**F-1 — the bulk uplink's `tick` field is not the universe tick.** The wire
doc says "The universe tick at append (D8)"
(`orrery_protocol/src/gateway.rs:378-379`), and the journal's idempotency
story is "keyed by `(entity, tick)`" (`persist.rs:200-202`). But the only
production writer fills it from a **client-local per-entity sequence counter
starting at zero**: `feed_uplink` does `let seq_num =
seq.next.entry(entity).or_insert(0); let tick = *seq_num;` and then
`tick: Tick::new(tick), … seq: tick`
(`orrery_persist_client/src/feed.rs:81-92`) — the same number is sent as both
`tick` and `seq`. The gateway journals it as received (`gateway.rs:8305+`
echoes `diff.tick` in nacks; nothing re-stamps it). Consequences for this
node: **today's bulk journal cannot be aligned with claim windows, replay
windows, or checkpoints by simulation tick** — its "(entity, tick)" key is
really "(entity, uplink-seq)". Idempotent resend still works (the counter is
monotone per entity), so nothing is corrupt; but any A7-adjacent design that
assumed the journal is tick-addressed (e.g. journal-as-rollback-substrate,
§4.3) would be building on a field that does not contain what its type says.
Same drift class as the `PersistId` block-grant doc comment A5 recorded:
present-tense wire documentation of an unbuilt behaviour. Flagged to A11 as
either a `feed_uplink` fix (stamp the real tick when the client has one) or a
doc correction (rename the semantic); which, is the owner's call.

**F-2 — event coverage exists, but only in per-game unit tests.** Mutation
X-A (§10) shows the precise shape of the goldens gap: an injected event-only
outcome sails through `chains_match_the_committed_golden` and the whole
battery (11 passed), through all of `orrery_core`, `orrery_conformance` and
`orrery_witness` — and dies against six hand-written assertions in
`orrery_games/tests/regolith.rs` (`assertion failed:
outcome.events.is_empty()` and friends). So the tree is not blind to events;
it is blind to events **in every instrument that would carry a migration
parity argument** (goldens, corpus, witness pipeline). The differential
harness has to inherit the unit tests' visibility, not the goldens' (§6).

---

## 2. What "rollback" is in this tree — three mechanisms, not one

The brief's question "what is the rollback unit?" presumes one rollback.
The tree operates three distinct mechanisms that only share a word, and the
unit answer differs per mechanism — conflating them is how a wrong unit gets
chosen. Inventory first; the decision is §3.

| Mechanism | Where | What rewinds | What never rewinds | Evidence |
|---|---|---|---|---|
| **Predictive resimulation** (client-side, presentation tier) | lightyear ring + `orrery_predict` budget/monitor | The mispredicting entity's predicted components, restored from the 16-tick ring at the authoritative tick, then re-stepped | Cosmetic state (never snapshotted, D13); anything outside the predicted set; the world | I11; `budget.rs:191-260` (ladder), X-E |
| **Authoritative correction** (canonical tier) | Adjudication verdict → `AdjudicatedState` → `AuthorityCorrectionInbox` | Nothing, in the rewind sense: the *corrected canonical state* is re-derived by isolated replay of the signed log and applied **forward** as an authoritative overwrite | The authority's own log — straight-line by construction (I10); committed durable effects | I13; docs/06:521 |
| **Durable recovery** (storage tier) | Checkpoint base + journal tail replay (`lsn > watermark`) | Nothing observable: recovery reconstructs the latest durable state; it never serves an older state to a live client | Critical (P2) rows: FDB RPO 0; corrections to them are compensating transactions, never rewinds (A5 IV-5) | I9; checkpoint/mod.rs:1-11 |

Two structural facts follow and are load-bearing for everything below:

1. **Canonical state is never rolled back anywhere in this system.** The
   phrase "rollback of canonical rules state" already got its honest gloss in
   A1 (§9 assumption 7 rider): it is a replay/correction story. The unit
   question is therefore a question about the *predictive* mechanism plus a
   question about what a triggered ECS host would need — not a question about
   rewinding the executor, the journal, or FDB.
2. **The cost model is already decided by shipped arithmetic.** The budget
   guard exists because worst-case resim is `window × step_cost` and the
   ladder (amortize → evict → snap) is what keeps a frame affordable
   (docs/05 §3; SnapNet's spiral-of-death arithmetic quoted there). Any unit
   larger than the predicted set re-imports the cost that ladder exists to
   cap.

---

## 3. Decision: the rollback unit

**Proposed (R-1): the rollback unit is the per-entity predicted set — grain
`(entity × its R1 components)`, scope the local predicted set, window the
9-tick ring, budget the existing ladder. World, island, and cell are rejected
as units. Canonical state stays correction-only; durable state stays
recovery-only; critical (P2) state stays compensation-only.**

Spelled out per candidate, each with the argument that beats it:

- **Entire simulation world — rejected.** The tree already rejected it in
  prose and practice: "Snapshotting only the predicted subset — never the
  world — is the point: this is what makes cost scale with interest size
  instead of world size, the exact failure of whole-world rollback"
  (docs/05:66). A world unit also rewinds entities whose authorities never
  mispredicted anything — under per-entity authority there is no single
  timeline to rewind *to*: entity A's authoritative update at tick T says
  nothing about entity B's. And under A3's adopted position there is no
  world-shaped canonical store to snapshot in the first place (I1). The
  brief's own worry ("Raw cloning or serialization of the complete `World` is
  unlikely to be acceptable") is thus answered structurally, not by
  benchmark: the design has no consumer for a world snapshot.
- **Authority island — rejected.** No island-wide history exists anywhere;
  corrections and residuals are keyed `(NodeId, PersistId)`
  (`monitor.rs:45-52` per A5 §2.3). An island unit would mean one entity's
  mispredict forces resimulation of every island co-member — cost scaling
  with island population, which is exactly the interest-vs-world failure
  again, one level down. Islands are an *authority* scope, not a *history*
  scope.
- **Spatial cell — rejected.** Cells are storage and interest keys
  (`world/` key carries the cell; A5 §2.7); nothing simulates per cell, and
  an entity's cell changes mid-window (rekey). A unit that migrates while
  the window is open is not a unit.
- **Entity set (the predicted set) — adopted as the scope.** It is what
  ships (I11), its cost is bounded by construction (the ladder demotes
  members until the replay fits, X-E-proven), and it composes with per-entity
  authority: each predicted entity reconciles against *its* authority's
  claims independently. Replay isolation makes this correct, not merely
  cheap: steps read neighbours only from snapshots and cross-entity effects
  travel as next-tick events (`executor.rs:106-142`), so re-stepping one
  entity does not require re-stepping its neighbours — the same property
  that makes single-entity adjudication valid (I13) makes per-entity
  rollback valid.
- **Component subset — adopted as the grain, inside the entity.** A5's R
  dimension is per-component and this document keeps it that way: within a
  rolled-back entity, `R1` components restore from the ring; `R0` components
  (ledger-backed P2 rows per IV-5, cosmetic per D13) are untouched. What the
  grain must **not** do is split the witnessed unit: the claim commitment is
  `state_hash` over the entity's whole `CoreState` (I2), so for
  verifiable-core entities the ring snapshots the quantized core state and
  RNG cursor as one block (docs/05 §3 already specifies exactly this) and
  restore is all-or-nothing at the entity. A partially restored core state
  would hash to a value no authority ever claimed — manufacturing the false
  deviation IV-2 exists to prevent.

**Under a triggered ECS host (A3 T1–T3), the unit does not change.** The
brief asks this node to compare component journals, archetype snapshots,
copy-on-write and lightyear's history as *mechanisms*; the comparison is in
§4.4, and its outcome is: the pilot's rollback substrate is a per-entity
`R1`-component ring keyed by `PersistId` — the shape lightyear's ring
already has — never a world snapshot, archetype clone, or COW world fork,
because the unit those mechanisms serve (the world) is the unit rejected
above. R-1 is storage-agnostic on purpose: it binds the executor store today
and any world-hosted store later.

**What R-1 changes in practice: nothing, today.** It is the current
behaviour, stated as policy so that the migration cannot drift away from it.
That is deliberate and is the same shape as A5's N-2: the value is in
binding the pilot, not in moving the tree.

---

## 4. The persistence strategy comparison

The brief demands a comparison: complete snapshots · component-level changes
· domain events · transaction journals · hybrid. The tree's first answer is
an inventory finding: **Orrery already runs a deliberate hybrid, with a
different strategy per tier, and each tier's choice is load-bearing.**

### 4.1 What exists, classified in the brief's vocabulary

| Tier | Strategy in the brief's terms | Mechanism | Evidence |
|---|---|---|---|
| Durable bulk | **Snapshot + journal hybrid** | Checkpoint base (copy-on-update, 20 s jittered, to FDB) + append-only journal delta; restore = base + tail replay | I9 |
| Durable critical | **Transaction journal** (the real one) | FDB serializable commits inside the intent envelope; RPO 0; receipts; anti-dupe single-ownership rows | docs/08 §2.2; `intent/mod.rs:152-155` |
| Evidence / witness | **Command journal + state commitments** | Signed per-entity input log (frames) + per-tick `state_hash` claims chained from `input_head`; replay re-executes the log | I2, I13; docs/06 §6 |
| Prediction | **Component snapshot ring** | 16-tick per-entity ring of predicted components (+ RNG cursor and quantized core state for verifiable entities) | I11 |
| Goldens/corpus | **State-hash chain fixtures** | blake3 chain over every per-tick state hash, committed | I4 |

The evidence tier deserves its name said plainly: it is **command sourcing
with state checkpoints** — the log stores *inputs* (cheap, canonical order
fixed by VC-2), and per-tick hashes commit the *result* without storing it.
That factorization is what makes adjudication both possible and cheap:
storage cost scales with input volume, verification cost with window length,
and neither with state size.

### 4.2 Complete canonical snapshots (as the primary strategy) — rejected

Snapshot-only persistence (serialize everything each interval) loses the
between-snapshot window (RPO = cadence), which the journal exists to close;
snapshot-only *rollback* was rejected in §3. Snapshots remain what they are
today: the base of the durable hybrid and the `t0` anchor of every evidence
bundle (`load_claimed_snapshot`, hash-verified before load — X-B). Nothing
about the migration changes this.

### 4.3 Component-level change journals (as the canonical/rollback substrate) — rejected for now, with the two named preconditions

The bulk journal is a component-diff journal already (`RecordKind::
ComponentDiff`). Could it double as the rollback/replay substrate — rewind by
reading the journal backward, replay by reading it forward? Not today, for
two mechanical reasons, both recorded rather than assumed:

1. **It is not tick-addressed.** F-1: the production writer stamps
   uplink-sequence numbers into the `tick` field, so journal positions
   cannot be joined to claim windows or ring ticks. Until the writer stamps
   real ticks, "journal-as-history" is unimplementable as specified.
2. **It makes no schema statement.** A5 G-3: `DiffUplink.payload` carries no
   `(ComponentTypeId, SchemaVersion)`; the actor resets overwritten bags'
   floors to v0 with a documented apology (`actor.rs:1300-1308`). A history
   you cannot version is a history you cannot migrate.

Both closures are already owned (G-3 by the A8/A11 framed-bag producer
package; F-1 flagged in §1.1), and once both close, journal-as-history
becomes *possible* — but §3 removes the need: the predicted-set unit is
served by the ring at strictly lower cost (16 ticks in memory vs a
disk-format read-modify path), and the canonical tier corrects by replay of
the *input* log, which is cheaper to store and already signed. Verdict:
keep the component journal as what it is — the durability delta — and do
not promote it to a history substrate.

### 4.4 Archetype snapshots and copy-on-write worlds (the ECS-pilot question) — rejected as mechanisms for a unit that was rejected

For a triggered dedicated world, the brief lists archetype snapshots and COW
as rollback candidates. Both are world-grain mechanisms: an archetype
snapshot clones column storage whose layout is world-history-dependent (I3 —
the same allocation-order dependence three probes reproduced), and a COW
fork preserves *the world's* past, not an entity's. Since the unit is the
per-entity predicted set (R-1), the pilot needs neither: it needs a
per-entity ring of `R1` components keyed by `PersistId`, which lightyear
already maintains for the presentation world and which a canonical host
would implement as a small `BTreeMap<PersistId, Ring<Snapshot>>` — the
executor's own storage discipline applied to history. Anything world-shaped
is not just unnecessary; it would re-import archetype layout into restored
bytes, which IV-7/WP-3 (§5) forbid from ever reaching an encoded artifact.

### 4.5 Domain-event journals (as canonical truth) — rejected; adopted as fixtures

Storing emitted `CoreEvent`s as the durable record of what happened is the
event-sourcing move. Against this tree it fails twice:

- **Events are derived, not primary.** The doctrine is written where the API
  lives: "an event-only effect is invisible to state-hash goldens and
  adjudication. A game whose materialization matters to adjudication must
  also record an own-state trace" (`ruleset.rs:280-284`). Adjudication
  verifies *state* commitments against *input* logs; an event journal would
  be a second, unverified account of the same history — and A2 CC-4 shows
  the tree already routes durable consequences of events through state
  counters and the intent envelope instead.
- **Event semantics are A6's, being decided right now.** Ordering, replay,
  dedup and idempotency of commands/events belong to #402 (sibling lane);
  a persistence strategy built on their semantics would decide another
  node's questions by fiat.

What survives is the narrow, valuable piece: **committed event-trace
fixtures as test instruments** (§6) — journals of events for *comparison*,
never for *authority*. That distinction keeps the single-source-of-truth
property: canon is state commitments + input logs; events remain derived.

### 4.6 Verdict

**Proposed (P-1): the migration introduces no new persistence strategy.**
The four-tier hybrid stands as inventoried in §4.1. What the #395 programme
changes is only *who produces the bytes* (modules declaring capabilities
per A5's P/R/W dimensions instead of one impl body) and *what additional
fixtures exist* (§6). Each tier's strategy maps onto the brief's module
model as: modules declare `(ComponentTypeId, SchemaVersion)` codecs and
P/R/W/N/A capabilities (A5 N-5/N-7); the kernel routes P1 through the
journal+checkpoint hybrid, P2 through the intent envelope, R1 through the
ring, W2 through frames/claims — and "the module system must not let modules
bypass transactional persistence invariants" (brief) is exactly A5's IV-3
plus A2 row 4's envelope ownership, already argued there.

---

## 5. Decision: the canonical witness projection

A4 and A5 both reserved "the canonical witness projection format" here. The
format below is written to be *already true* of the current tree (so
adopting it costs nothing today) and *binding* on any future host (so the
pilot cannot drift). Plain math first, rules after.

The projection of one entity `e` at tick `t`:

```text
bytes(e, t)  = CoreCodec::encode( quantize( state(e, t) ) )     # declared codec, fixed field order
hash(e, t)   = blake3( bytes(e, t) )                            # the value a StateClaim commits to
```

The chain a claim window folds (what `input_head` anchors, docs/06 §6):
per-entity, per-tick, over the entity's logged inputs — state hashes are
committed by claims, inputs by the chain; the two meet at `verify_bundle`.

Multi-entity aggregates, wherever one is ever needed (corpus chains, a
checkpoint digest, any future world digest):

```text
world_digest(t) = blake3( concat( for id in sort_ascending(ids):  id ‖ bytes(id, t) ) )
```

### 5.1 The rules (proposed as normative)

- **WP-1 — the unit of witness commitment is one entity-tick.** Claims,
  frames, bundles and replay all address `(PersistId, Tick)`; nothing
  commits to a multi-entity hash on the wire. This is today's shape (I2,
  I13) kept deliberately: it is what makes single-entity adjudication, the
  witness's one-executor-per-watched-entity model
  (`witness.rs:406-421` per A1 §5.3), and R-1's per-entity restore all
  consistent with each other.
- **WP-2 — entity order is `PersistId` ascending; cross-grid,
  `(GridId, PersistId)` ascending.** Any enumeration of entities that feeds
  bytes into a hash, a golden, a fixture or an emitted artifact sorts first.
  This is A5 N-2.3 restated as the projection's ordering clause, and it is
  the exact mitigation all three iteration-order probes validated (I3).
  Today's executor satisfies it structurally (`BTreeMap` keys; `entities()`
  documents "in `PersistId` order", `executor.rs:97-100`).
- **WP-3 — component order is `ComponentTypeId` ascending, each slot framed
  `(ComponentTypeId, SchemaVersion, payload)`.** Today a `CoreState` is one
  declared codec and this clause is vacuous for it; it binds the day state
  becomes per-component (A3 T1 is precisely that trigger). The framing is
  the at-rest bag's shape (`orrery_persistd/src/schema.rs:48-66`) so the
  witness projection and the persistence encoding cannot diverge — "a claim
  commits to exactly what replication and persistence saw" stays one
  sentence with one meaning.
- **WP-4 — quantize before hash, always** (VC-7; A4 stage rule S4 ≺ S5).
  Note honestly: mutation X-C (§10) shows this ordering is currently
  **unpinned by any test** — it survives because every in-tree state stores
  continuous fields as lattice integers already. The clause stays; the
  missing test is named in §10.
- **WP-5 — no engine artifact may reach the projection.** No `Entity` bits,
  `ComponentId`s, `FnsId`s, archetype or row indices, reflection names, and
  no bytes produced by iterating any world container (A5 IV-7/N-6). For the
  executor store this is structural (the defining crates are gate-held
  Bevy-free, A5 §2.2); for a world-hosted store it is exactly what A4's
  E-M3 differential harness must prove per commit.
- **WP-6 — the projection is versioned.** A `projection_version` integer,
  bumped on any change to WP-2/WP-3 framing, carried in the manifest (format
  A8's) beside `RulesetId` and the schedule digest. Today's value is 1 and
  describes the shape above. Without this, a projection change would
  present as mass deviation — the false-conviction failure IV-2 names.

### 5.2 Why this is shown order-immune rather than assumed

The acceptance bar was: no reliance on raw ECS world serialization without
evidence that archetype order, component insertion order, entity allocation
order and hash-map iteration cannot reach the hash. The evidence, by store:

- **Current store — immunity is structural.** Nothing iterates a container
  into `hash(e,t)`: the input is one entity's own state (I2), and the one
  map in the path is keyed `BTreeMap` whose order is `PersistId` by type
  (I1). Hash-map iteration cannot reach gated sources at all (VC-4 clause,
  mutation-proven A1 M3).
- **World-hosted store — immunity is a per-commit proof obligation, with
  the hazard demonstrated and the mitigation demonstrated.** Three
  independent probes reproduced insertion-order-dependent query iteration on
  the pinned `bevy_ecs 0.19` (I3); the same three showed the WP-2 sorted
  projection agreeing across permutations (A3 P1/P2 "canonical AGREES |
  naive DIFFERS"; A4 E-2 one hash across six executor/order cells). The
  named check is A4's E-M3 (`projection-order-permuted` corpus case):
  permuted insertion orders must produce equal sorted-projection hashes
  *and* match the executor-computed chain. Per A4 Tier H, that harness is a
  precondition of admitting the host, not a follow-up.
- **The stability trap is refused.** Observed agreement of a *naive*
  projection across runs proves nothing (A3 P3: 200/200 stable and still
  unspecified); E-M3 therefore deliberately asserts nothing about naive
  folds. This document inherits that discipline: WP-2 is the rule because
  sortedness is provable; stability is not.

### 5.3 What the projection excludes, said once

Excluded from witness bytes, each with its class: presentation and cosmetic
state (P0/W0 by default — A5's zeros fail closed); `EphemeralId` entities
(structurally unpersistable, A5 IV-4/X1); materialization descriptions
(`ruleset.rs:280-284` — own-state traces carry what matters); lightyear ring
contents and `VisualCorrection` residuals (presentation tier, I12);
`UplinkSeq` counters and every other in-memory index (A5 N-2.2: rebuildable
projections are never encoded). Events are excluded from *claims* today and
this document does not move them onto the wire — the event fixture chain
(§6) is a test instrument; putting event commitments into `StateClaim` would
be a protocol change and is flagged to the owner, not proposed.
