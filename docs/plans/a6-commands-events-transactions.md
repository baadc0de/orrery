# A6 — Commands, events and transactions (#402)

**Status:** decision proposal for the #395 planning tree · **Date:** 2026-08-25 ·
**Tree:** `docs/402-a6` at `3195583d` · **Parents:**
[#402](https://github.com/baadc0de/orrery/issues/402) ←
[#395](https://github.com/baadc0de/orrery/issues/395) · **Builds on:**
[A1](a1-ruleset-architecture-map.md), [A2](a2-kernel-game-module-ownership.md),
[A3](a3-simulation-host-comparison.md) (+ its
[second opinion](a3-simulation-host-second-opinion.md)),
[A4](a4-deterministic-execution.md) (PR #418, in flight),
[A5](a5-identity-and-capabilities.md) · **Source brief:**
[docs/plans/ruleset-ecs-migration-brief.md](ruleset-ecs-migration-brief.md)
§Commands, events, and queries

Ordering, replay, rollback and idempotency are undefined for an ECS-shaped
design. This document defines them for six message classes — external
commands, internal deterministic commands, domain events, persistence events,
presentation events, diagnostics — states explicit rules for immediate versus
stage-delimited effects, and takes a verdict on the own-state discipline the
tree enforces today. It also writes out three sequences end to end:
command-to-persistence, rollback, and a cross-module integration.

Two boundaries this document holds: A4 fixed *ordering and delivery timing*
and explicitly deferred "replay behaviour, deduplication/idempotency … and
volume bounds" here (`a4-deterministic-execution.md` §3.5, §8); and the
rollback **unit** stays with A7 (#403) — every rollback statement below is
about message semantics, which hold under any unit A7 picks.

Method, continuing A1–A5:

- Every claim cites a file and line opened on this tree today. Where this
  document asserts a property is *enforced*, the guarded stage was broken,
  the named check recorded with its real result line, the change reverted,
  and the pass re-confirmed (§10). Two mutations **survived**; both are
  recorded as coverage findings rather than discarded.
- What **exists today**, what is **designed but unwired**, what is
  **proposed here**, and what belongs to another owner never share a sentence.

---

## 1. Ground truth inherited and re-verified on this tree

Each finding below was re-opened on this tree before use. Line numbers are
this tree's.

| # | Finding | Evidence |
|---|---|---|
| G1 | **Own-state discipline exists and is gate-enforced.** `step` sees own state + ordered inputs + RNG only (`ruleset.rs:257-262`); cross-entity effects travel as emission-ordered events consumed next tick (`ruleset.rs:196-201`); snapshot isolation removes own state before the step so no same-tick read of others is possible (`executor.rs:104-118`; mutation-proven by A3 F-2); live neighbour reads are banned in rules crates with the rationale "at replay every neighbour read returns `None` and a rule that branched on one adjudicates differently than it executed" (`core-gates.sh:126-139`) | Opened this tree |
| G2 | **Routing is game-owned and harness-side.** "The executor deliberately does not route events, because routing is a property of the game's rules" (`game.rs:10-12`); `Game::deliver(&event) -> Option<(PersistId, CoreInput)>` maps one event to its target's next-tick input (`game.rs:151-153`). The reference routing loop lives in the scenario harness: delivered inputs compose *first* in the target's input vector, then player orders ("arbitrary but fixed", `scenario.rs:209-214`); entities step in ascending `PersistId` order (`scenario.rs:135`, BTreeMap keys); `pending = delivered` swaps after the tick (`scenario.rs:253`) | Opened this tree |
| G3 | **Target-side validation is real.** `Order::Damage` carries `amount, from, from_pos, from_vel, from_weapon, flight_ticks` (`regolith/order.rs:121-129`); the *target* resolves the projectile in its own step with its **own** tick RNG (`projectile_resolution(..., rng)` at `regolith/mod.rs:330-340`), applying shield/hull only on `Hit` (`:377-393`). The attacker separately carries a monotone `damage_dealt` own-state counter so inflated rolls are "adjudicable at the attacker" (`regolith/state.rs:35-36`) | Opened this tree |
| G4 | **Inputs are logged before execution and replayed from the log.** The bot logs exactly what it is about to apply *before* stepping (`bot.rs:715-721`, `chain.log_inputs` at `chain.rs:134`), logs the post-step hash (`bot.rs:734-736`), and cuts claims from pre-tick state ("the ordering is the whole correctness of this", `bot.rs:1086-1093`). Replay collects logged inputs per absolute tick while verifying frame signatures (`replay.rs:172`, decode at `:230`) and steps empty ticks too (`:240-241`, `:149-151`) | Opened this tree |
| G5 | **Persistence idempotency is keyed, not best-effort.** Journal records are "idempotent — keyed by `(entity, tick)` with last-writer-wins per component within an entity's single-writer stream — so unacked diffs can be resent on reconnect" (`persist.rs:198-203`); kinds are `ComponentDiff/TerrainDelta/Spawn/Despawn/Rekey/CheckpointMark` (`persist.rs:135-150`); intent admission caps ops at 64, args at 4 KiB/op and 64 KiB/intent, attestations at 16 (`intent/mod.rs:182-202`), with the gateway check only a fast filter — "the FDB transaction remains the sole authority" (`intent/mod.rs:151-161`); exactly two cluster-interpreted op ids exist as *reservations* (`LEDGER_CREDIT_OP = 0` at `:204-210`, `LEDGER_ITEM_TRANSFER_OP = 2` at `:215-257`) | Opened this tree |
| G6 | **Witness evidence re-delivery is structurally deduped.** A frame entirely behind the fold watermark is a duplicate returning `Ok(vec![])` before anything re-folds (`witness.rs:847-861`); the deferral buffer keys on `(subject, first_tick)` so a re-served frame *replaces* rather than accumulates (`witness.rs:1023-1034`, counter doc `:169-175`); coverage counts shown ticks from each watch's *advance* "so a repair re-delivering a range is not counted twice" (`witness.rs:117-127`) | Opened this tree |
| G7 | **A4 fixed delivery timing and left the rest here.** Stages S0 SealInputs → S1 Deliver → S2 Step → S3 Record → S4 Quantize → S5 Claim → S6 Materialize → S7 Emit (`a4-deterministic-execution.md` §3.2); "delivery at S1 of t+1 and never earlier; observers/hooks do not exist in the canonical path" (§3.5); deferred structural changes flush at S6 only, first-writer-wins (§3.4). Its threat model names event/observer ordering (T7), deferred structural-change order (T8) and same-tick cross-entity reads (T10) as the classes A6's rules must close | PR #418 head, read in full |
| G8 | **A5's dimensions govern channel membership.** Every component carries P/R/W/N/A; zeros fail closed; eight invalid combinations include W2-without-A1 (no signer, no verdict) and P2-without-A1/A3 (bypasses read-check-write) (`a5-identity-and-capabilities.md` §5.2, §5.4). Message-class rules must not reopen those closures — they consume them | Read in full |
| G9 | **Corrections flow forward, never rewind durable rows.** Verified corrections queue through `AuthorityCorrectionInbox` (`correction.rs:46-53`, facade `queue_authority_corrections` at `orrery/src/lib.rs:336`) and reconcile into predicted/presented state; committed ledger credits reverse only by compensating transaction (A5 §5.3, IV-5) | Opened this tree |

One briefing correction, per the standing rule: the issue text says "`Order::Damage`
carries `from_pos` and the target adjudicates it". Verified true, with one
sharpening — the target adjudicates *geometry and application* (its RNG, its
shield/hull), while the *amount* is attested by the attacker's own monotone
counter and caught at the attacker by replay (G3). The discipline is
two-sided, not merely target-side.

---

## 2. The six message classes

### 2.1 Definitions (semantic), and one deliberate mechanical collapse

| Class | Definition | Where it lives today |
|---|---|---|
| **External command** | A request entering canonical execution from outside the host seam: a player's `Order`, a service's intent op, a replay harness injection. It has no producer inside the tick | `R::CoreInput` entries sealed into the authority's log before the tick runs (G4); intent ops through the gateway (`intent/mod.rs:151-161`) |
| **Internal deterministic command** | A request one rule addresses to another entity, scheduled by the simulation itself | **No separate mechanism exists.** Today an internal command *is* a domain event routed by `Game::deliver` into the target's next-tick input vector (G2) |
| **Domain event** | The deterministic output of one step; carries full description of what happened (`Outcome::DamageDealt` with positions, weapon, amount) | `R::CoreEvent`, emitted in emission order (`ruleset.rs:193-208`) |
| **Persistence event** | A durable change or transaction result that outlives the process: journal record, uplink diff, tombstone, committed intent op | `JournalRecord` kinds (`persist.rs:135-150`); `DiffUplink`; intent receipts with minted `PersistId`s (A5 §2.4) |
| **Presentation event** | An engine-facing consequence with no authority over canonical state: mirror writes, replicated components, interpolated frames | Bot mirror write (`bot.rs:738-744`); replicon/lightyear surface; exterior bridge frames |
| **Diagnostic** | Tracing, metrics, counters, conformance output. Observes anything; is observable in nothing | Witness coverage counters (`witness.rs:110-179`); scenario `Play.events` tally (`scenario.rs:148-151`) |

**Decision (proposed): classes 2 and 3 stay mechanically unified.** An
internal deterministic command is a domain event whose *source module
declares* it as a request rather than a report; downstream — ordering,
buffering, replay, rollback, idempotency — the two are indistinguishable.
Three reasons, each grounded above:

1. **One channel, one replay story.** Events are never stored; they are
   re-derived from logged inputs during replay (G4). A second, immediate
   internal-command channel would need its own log, its own ordering
   discipline, and its own replay reconstruction — three new artifacts to keep
   bit-identical across authority, witness and adjudicator, for a distinction
   nothing consumes.
2. **Immediate dispatch is the failure A4 banned.** Observer-style cascades
   are threat T7 ("immediate observer/hook cascades whose recursion depth and
   interleaving are unspecified"); A4's §3.5 states observers "do not exist in
   the canonical path". An immediate internal-command channel is an observer
   cascade with another name.
3. **The tree already tried the alternative and closed it.** Live neighbour
   reads are the same urge — reach across now instead of waiting a tick — and
   the neighbour ban exists because no replay can reconstruct what the read
   saw (`core-gates.sh:126-132`). The collapse onto next-tick inputs is the
   positive form of that ban.

Cost of the collapse, stated honestly: latency. Every cross-entity effect
costs one tick. That is the price of isolated single-entity replay (A3 E-8)
and it is already being paid everywhere in the tree.

### 2.2 Transaction boundaries (what is atomic where)

"Transactions" in this design are four different atomicity claims at four
tiers; conflating them is how dupe bugs are born:

| Tier | Atomic unit | Mechanism | Evidence |
|---|---|---|---|
| Rule step | One entity's own-state mutation + event emission | The step mutates a removed copy; quantize-then-hash then reinsert (`executor.rs:116-128`). No partial state ever escapes a step | Opened |
| Canonical tick | Per-entity claim chain link | Input seal (S0) → hash (S5) → fold into chain; claims commit pre-tick state (G4) | Opened |
| Durable entity state | `(entity, tick)` journal key under lease fencing | Last-writer-wins per component within a single-writer stream; resent diffs converge (G5) | Opened |
| Critical/ledger rows | The FDB intent transaction | Sole authority; gateway check is advisory (G5); anti-dupe = single ownership row (`persist.rs:183-196`) | Opened |

A rule step is deliberately **not** a transaction across entities — that is
the own-state discipline again. Cross-entity atomicity exists only at the
durable tier, inside the cluster's transaction, where it belongs (A2 row 4).

---

## 3. Rules per class

Each class answers the six required questions. Everything here is stated for
the canonical tier first; W0/cosmetic state follows presentation-class rules
regardless of which module owns it (A5 §5.4 profiles).

### 3.1 External commands

| Question | Rule |
|---|---|
| Ordering | Total order fixed by the executing authority at S0, in arrival order under its lease; iteration is log order, never re-sorted (VC-2, `ruleset.rs:149-157`). Order is semantic — `input_order_changes_the_outcome` pins it (`executor.rs:504-515`) — so admission order is part of the game's law |
| Buffering | Transport receive buffers → S0 seal per tick. Late arrivals wait for t+1 (A4 S0). Admission bounds are kernel-side and cheap: volume/args/attestation caps refuse before any FDB round trip (G5) |
| Replay | Replayed verbatim from the signed log; logged pre-execution so a cheat cannot manufacture a flattering log (G4). Empty ticks still step (`replay.rs:240-241`) |
| Rollback | Commands are immutable history. Prediction resimulates *over* them; adjudication re-executes *over* them; neither edits the sealed log. Correction replaces state, never inputs |
| Idempotency | Below the seam: transport dedup is a net-layer concern and must happen **before** S0. Once sealed, every entry is real — a duplicated order is two orders. At the durable tier, external intents dedup by op-id reservation and minted receipts (G5, A5 N-3) |
| Cyclic dependency | Cannot cycle: external sources enter once, at S0, and have no back-edge into their submitter except via later domain events |

**Proposed normative (C-1):** *deduplication of external commands happens only
below the seam or by durable op-id; never by content matching inside the
tick.* Two byte-identical commands in one tick are two commands — Regolith's
double-tap fires twice, and content-based dedup would silently break rules
that legally repeat.

### 3.2 Internal deterministic commands

Identical to §3.3 in every mechanical respect (§2.1 decision). The only
class-specific rules:

- A command names its target explicitly and routes through the game's
  `deliver`; unroutable events (`deliver → None`) die at the emitter without
  effect — legal, and the current behaviour for materialization-carried
  events like `SpawnPickup` (`regolith/mod.rs:1145-1148`).
- Provenance travels inside the payload (`from`, `from_pos`, …) exactly as
  `Order::Damage` does (G3). The target validates plausibility from payload +
  own state; it cannot interrogate the emitter's live state.

### 3.3 Domain events

| Question | Rule |
|---|---|
| Ordering | Emission order within one step is determinism — "`Vec`, never a set" (`ruleset.rs:196`). Cross-entity total order = producer step order (ascending `PersistId`) × emission order within each producer (G2 loop). Delivery order into a target's input vector preserves producer order; delivered-before-player composition is fixed convention, not luck (`scenario.rs:209-214`) |
| Buffering | Held for the remainder of the producing tick in a per-target queue keyed by `PersistId`; applied at S1 of t+1; strictly never same-tick (A4 §3.5). The reference buffer is a `BTreeMap<PersistId, Vec<CoreInput>>` (`scenario.rs:206`) |
| Replay | **Events are derived, never stored.** Only inputs and hashes cross the wire as evidence; every consumer re-derives events by re-executing. This is why event duplication is structurally impossible: there is no event transport to duplicate |
| Rollback | Re-derived during re-execution. Because targets' effects also arrive as logged inputs, rewinding a window rewinds the routed consequences automatically — no event-level undo machinery exists or is needed |
| Idempotency | By position, never by value. An event's identity is *(emitter, emitting tick, emission index)* inherited from its log position as a delivered input; identical payloads are distinct events. Content-based dedup would be nondeterministic under collisions and would hide legal repeats (C-1's mirror image) |
| Cyclic dependency | **Broken by construction.** Same-tick cycles cannot exist (snapshot isolation, G1). Cross-tick cycles become discrete iterations: A→B→A costs two ticks per lap and either converges to a fixed point, oscillates deterministically, or diverges visibly in state hashes and stage-1 checks. Nothing blocks, so no deadlock exists. Termination is *not* required for determinism — divergence detection (D10) is the safety net, and the volume caps below bound how fast a pathological loop can burn budget |

### 3.4 Persistence events

| Question | Rule |
|---|---|
| Ordering | Append-only per node (`lsn` monotonic, `persist.rs:207`); per entity, single-writer under lease fencing with `(entity, tick)` last-writer-wins convergence (G5); epoch fences reject zombie writers (A5 §2.6) |
| Buffering | Client-side uplink scheduler with upload budgets (`UploadBudget`, used at `bot.rs:40,553`); unacked diffs are resent on reconnect — safe precisely because records are idempotent by key (G5) |
| Replay | Persistence events **are** the durable truth: recovery replays journal after checkpoint (D19/D20/D23). But they are evidence-opaque: the cluster stores bytes it cannot interpret (A2 row 6), so "replay" here means fold-and-restore, never re-execute |
| Rollback | Never rewound. Entity-state corrections flow forward as new authoritative rows (`AdjudicatedState` → correction inbox, G9); ledger reversals are compensating transactions (A5 IV-5). A rollback window may therefore leave durable traces written during the window — they were valid under the then-holder's fence, and correction supersedes rather than erases |
| Idempotency | `(entity, tick)` key for bulk rows; op-id reservation for transactional ops; minted `PersistId`s returned in receipts so a retried intent cannot double-mint (G5, A5 §2.4); tombstones cancel on respawn (`actor.rs:1319-1325` per A5 §2.6) |
| Cyclic dependency | One-way: sim → store. The only store→sim edges are load-at-startup and the correction inbox, both staged outside the tick loop. A persistence event can never influence the tick that produced it |

### 3.5 Presentation events

| Question | Rule |
|---|---|
| Ordering | Best-effort, AOI-scoped; interpolation absorbs reorder (docs/05). No ordering rule is load-bearing because nothing canonical reads them |
| Buffering | Mirror buffers and replication queues; bounded by interest set, which is deliberately bounded (A3 second opinion P-1 note on `interest.rs`) |
| Replay | Never replayed; regenerated from canonical state. A rejoined client rebuilds by snapshot + catch-up, not by event history |
| Rollback | Discarded and regenerated post-correction; lightyear reconciliation absorbs the visual snap. Presentation must not attempt its own undo logic — regeneration from corrected canonical state is the undo |
| Idempotency | Overwrite semantics throughout: a mirror write is assignment, not accumulation. Duplicate frames converge |
| Cyclic dependency | Presentation reads canonical state; canonical code reading presentation state is the leakage A3 rejected V2 for and is structurally impossible while canonical state lives outside any app world (A3 E-1). Under a triggered ECS host, Tier H keeps presentation components out of the canonical world entirely (A4 §5.2) |

### 3.6 Diagnostics

| Question | Rule |
|---|---|
| Ordering | Unordered, sampled, best-effort. Counters may interleave freely — nothing replays them |
| Buffering | Local; sampling windows; dropped freely under pressure |
| Replay | Never inputs to anything canonical. Conformance outputs are recomputed from canonical bytes each run, never accumulated across runs |
| Rollback | Annotated, not rewound: a counter incremented during a window that later rolls back stays incremented, with the report/window markers explaining it. Rewinding diagnostics would mean diagnostics are state — the class boundary forbids it |
| Idempotency | Not required; consumers treat values as observations, not facts |
| Cyclic dependency | Diagnostics observe everything including themselves; affect nothing. The one hard rule: **no diagnostic value may be readable by a rule**, which VC-8 already enforces negatively (no ambient inputs into gated crates, `core-gates.sh:103-105`) |

---

## 4. Immediate versus stage-delimited effects — explicit rules

A4's stage model (S0–S7, G7) supplies the delimiters; these rules decide what
may happen where, and what each violation costs. "Immediate" means *visible to
another consumer within the same tick*; "stage-delimited" means *produced in
one stage, visible to others only at a named later boundary*.

- **R1 — All cross-entity canonical effects are stage-delimited to the next
  tick.** Produced in S2, delivered at S1 of t+1 (A4 §3.5). There is no
  same-tick cross-entity visibility at any capability tier.
  *Violation cost:* the adjudicator installs one entity against an empty
  neighbour map (`replay.rs:106-130`), so an immediate effect adjudicates
  differently than it executed — the exact failure the neighbour ban names
  (`core-gates.sh:126-132`). Enforced today by snapshot isolation + gate; A4
  T10 carries it into any ECS future.

- **R2 — Within-entity immediate effects are unlimited and unmediated.** A
  step may read and write its own state freely, in any order, with immediate
  visibility — that is the whole content of own-state (R4 below). Shield then
  hull then disabled flags in one tick is correct precisely because no other
  entity can observe the intermediate values (`regolith/mod.rs:377-393`).

- **R3 — Structural changes are stage-delimited to S6 of the producing tick,
  and invisible to every S2 of that tick.** Materializations install
  first-writer-wins in description order = emission order
  (`executor.rs:144-157`); a child born at T begins stepping at T+1
  ("never halfway through their birth tick", `scenario.rs:202-204`). The
  corpus's shared-vs-isolated chain equality pins this mechanically
  (`corpus.rs:38-49`, `:95-102` per A4 §3.2). *Violation cost:* birth-tick
  ordering would depend on entity visit order, which replay cannot reproduce.

- **R4 — Own-state writes are the only immediate writes, and they are
  confined to the owning step.** No deferred command queue exists for
  canonical state today; a rule's mutations land before its events are even
  collected (`executor.rs:122-134`). Under an ECS host this becomes A4 §3.4's
  flush-at-S6-only rule for *structural* changes; value changes stay
  immediate-in-step.

- **R5 — Persistence leaves after claims, never before, and never feeds back
  into the producing tick.** Uplink sources read post-S4 quantized state;
  journal records carry post-step ticks; the correction inbox drains outside
  the schedule run (A4 §3.9). *Violation cost:* a store→sim back-edge inside
  the tick makes outcomes depend on storage latency, which replay cannot
  reproduce.

- **R6 — Presentation reads only post-S4 state** (quantized, claim-committed
  bytes), so what a player sees is exactly what a claim commits to (VC-7
  rationale). Presentation never writes canonical state (§3.5).

- **R7 — Diagnostics observe any stage; are written by none** (§3.6).

**Summary rule:** *immediate is allowed only where the audience is the actor
itself (R2) or nobody canonical (R7); every cross-audience effect waits for a
named stage boundary.* The design has exactly three boundaries worth naming —
next-tick delivery (effects), end-of-tick structural flush (spawns), post-tick
drain (persistence/corrections) — and resists adding a fourth, because each
additional boundary multiplies the ordering surface replay must reproduce.

---

## 5. Verdict on own-state discipline: preserved and extended

**This proposal preserves the discipline; it does not replace it.**

The argument, from enforcement facts rather than taste:

1. **The discipline is what isolated replay is made of.** The adjudicator's
   guarantee — re-execute one entity against an empty neighbour map and its
   verdict binds any machine, months later — holds because a step's entire
   input surface is `(own state, logged inputs, seeded RNG)` (G1). Every
   relaxation (live neighbour reads, immediate cross-entity commands, shared
   mutable world rows) converts that guarantee from structure into policy:
   replay would need a `NeighborFrame` producer that does not exist anywhere
   in the tree (A1 §5.6; G6's dedup machinery shows how much transport
   subtlety evidence re-delivery already needs *without* neighbour payloads).
2. **No replacement on offer meets the epic's bar.** The alternative — allow
   cross-entity reads backed by recorded neighbour frames — requires a
   producer, a replay consumer, and a strength proof that the new path
   adjudicates as it executed. None of the three exists; the gate's comment
   states the residual precisely (`core-gates.sh:126-132`). A weaker
   replacement that passes is worse than the current ban (epic constraint).
3. **ECS makes the discipline more necessary, not less.** Multi-entity queries
   become silently expressible under ECS storage — A4's T10 says exactly this
   — so the discipline must get *stronger* enforcement under migration, not
   weaker. A4 Tier H already requires hosts to expose single-entity step
   semantics to witnesses/adjudication as non-negotiable (`a4-deterministic-
   execution.md` §5.2).
4. **What extends is scope, not kind.** Two extensions, both conservative:
   - **New classes inherit it.** Persistence events may not feed their
     producing tick (R5); presentation may not feed anything canonical (R6);
     diagnostics affect nothing (R7). The discipline generalizes from "rules
     don't read neighbours live" to "no class crosses a tier boundary
     backwards within a tick".
   - **Enforcement surface widens.** Today's gate watches a crate list whose
     coverage is the list itself — `orrery_witness` carries 530 bevy
     references past a green gate (A4 §1.2, re-verified there and cited, not
     re-derived here). A4's role-discovery Tier V closes exactly that hole;
     adopting it strengthens this discipline's enforcement without touching
     one rule.

What would change this verdict, pre-registered rather than rhetorical: a
shipped, mutation-proven `NeighborFrame` producer + replay consumer + a
demonstrated rules behaviour that next-tick delivery cannot express (e.g. a
genuinely simultaneous interaction both parties must resolve identically
without a referee entity). Absent all three, next-tick composition is the
discipline, and §3.3's cycle answer is why even "simultaneous" interactions
compose safely across two ticks.

---

## 6. Sequence S1 — command to persistence, end to end

Regolith: attacker **A** (`PersistId(1)`) fires at victim **B** (`PersistId(2)`),
B's hull crosses zero. Every message class appears at least once. Stage labels
are A4's (G7); the authority holds both leases.

```text
T-1   [external]   A's pilot produces Order::Fire{target:2}; host logs it into
                   the tick-T input record BEFORE executing (bot.rs:715-721).
                   Net retry of the same packet was deduped below the seam;
                   a second sealed entry would be a legal second shot (C-1).
T     S0           Input log for T sealed: A gets [Fire]; B gets [].
T     S2 (A)       A's step: lock already held; cooldown clear; attacker RNG
                   rolls damage; DamageDealt{attacker:1,target:2,amount,
                   attacker_pos,…} pushed in emission order (mod.rs:301-320);
                   monotone damage_dealt counter advances in own state
                   (state.rs:35-36). Own-state write only — R2.
      S4/S5        A quantizes; state_hash(T) claimed; chain folds.
      S7→route     deliver(DamageDealt) → (target=2, Order::Damage{amount,
                   from, from_pos, from_vel, from_weapon, flight_ticks})
                   (mod.rs:1076-1094). Buffered for t+1 — R1.
T+1   S0           B's input vector: [Damage] delivered-first, then any
                   player orders (scenario.rs:209-214 convention).
      S2 (B)       B validates in its own step with its OWN rng:
                   projectile_resolution(origin, own.vel, radius, alive,
                   from_pos, from_vel, weapon, flight, rng) → Hit|Miss|
                   InFlight|Break (mod.rs:330-340). On Hit: shield absorbs,
                   hull drops to 0 → disabled=true, respawn_in set,
                   events push Destroyed{by:1} then LockBroken — emission
                   order preserved (mod.rs:377-393). Two-sided verification:
                   geometry here, amount attested by A's counter at A's
                   replay (G3).
      S5           B claims post-tick state. Witness-set peers receive the
                   frames and re-execute both entities independently —
                   [diagnostic] coverage counters advance; no canonical effect.
      route        deliver(Destroyed) → (1, KillCredit); deliver(LockBroken)
                   → (1, LockBroken) (mod.rs:1106-1133).
T+2   S2 (A)       A consumes [KillCredit, LockBroken] as ordinary inputs;
                   kills/score advance in A's OWN state — the credit exists
                   nowhere else, so isolated replay of A reproduces it from
                   the log alone (test: kill_credit_is_log_delivered_and_
                   replays_from_the_killers_input, regolith.rs:199-278).
      persist      Persist-client diff of B's post-T+1 components uploads as
                   DiffUplink under lease fencing; cell actor writes journal
                   row keyed (entity=2, tick=T+1), last-writer-wins per
                   component (G5). FAILURE INJECTION: the uplink is unacked
                   and resent after reconnect — same key, converged value,
                   idempotent by construction. No dupe path exists because
                   the key, not the payload, arbitrates.
```

Where each guarantee binds: ordering (log order + emission order + `PersistId`
step order); buffering (per-target queue, one-tick latency); replay (inputs +
hashes only; both entities' outcomes re-derived in isolation); rollback
(nothing durable yet — but if the window were disputed, §7 applies verbatim);
idempotency (sealed-entry-is-real at T; `(entity,tick)` at the store).

---

## 7. Sequence S2 — rollback

The unit is A7's (#403); this sequence shows the *message semantics*, which
hold whether A7 picks entity-window, island or component granularity,
because canonical rollback here is **re-execution over immutable logs plus
forward correction** — not state rewinding.

Setup: B is witness-adjudicated (W2 ∧ A1, A5 §5.4). B runs a modified build
claiming the honest `RulesetId` (the tamper model, `game.rs:30-36`).

```text
1.  [domain]     B executes ticks T..T+k with inflated damage rolls; every
                 hash is deterministic for B's ACTUAL code — determinism is
                 not honesty (A4 §7.1). Claims stream to witnesses, signed.
2.  [diagnostic] Witness stage 1c re-executes B's logged inputs on its own
                 per-entity executor; computed hash ≠ B's claim at T+j;
                 check_pending_claims arms the audit window (pipeline pinned
                 by detection tests, A1 M8).
3.  [domain]     Audit window opens at newest demonstrably-agreed claim,
                 closes at the dispute. Re-execution walks logged inputs
                 per tick (replay.rs:172,230-241): all domain effects inside
                 the window are RE-DERIVED — routed damage to A, kill
                 credits, everything — because delivery is a pure function
                 of the log (§3.3). No event-level undo exists or is needed.
4.  [external]   The bundle (subject-signed claims + input log) adjudicates
                 cluster-side via verify_bundle; verdict Confirms{at:T+j}.
                 External commands inside the window are never edited —
                 history is history (§3.1).
5.  [persistence] Rows written during the window stay; correction flows
                 forward: AdjudicatedState carries replayed canonical state
                 (adjudication.rs:282-298 per A1 §5.4) → correction inbox
                 (correction.rs:46-53) → new authoritative journal rows at
                 later ticks supersede under the current lease fence. A
                 committed ledger credit inside the window reverses only by
                 compensating transaction (A5 IV-5) — never by rewind.
6.  [presentation] Corrected state re-mirrors; lightyear reconciliation
                 snaps the view. Presentation performs no undo logic (§3.5).
7.  [diagnostic] Strike accounting records the disposition; shadow-mode
                 default counts without filing (witness.rs:56 per A1 §5.3).
```

Rollback properties worth stating: the window's *events* need no reversal
machinery (step 3 derives them fresh); its *durable* rows are never erased
(step 5 supersedes); its *presentations* regenerate (step 6). The classes that
survive a rollback unchanged are exactly external commands and persistence
history — which is why they, alone, are append-only.

---

## 8. Sequence S3 — cross-module integration: damage × inventory × docking

A2 CC-1's corner case walked through message semantics (ownership analysis is
A2's; this is the flow). Craft **C** is docked to station **S**, owned by
peer **P**; attacker **A** destroys C; C's cargo holds item X.

```text
T      [authority]  Docking is an authority fact, not a rule input: step has
                    no lease parameter (ruleset.rs:257-262). If docking
                    changes who may submit for C, that is enforced at the
                    admission seam before S0 — it never reaches rules as
                    live state. Rules see at most a reflected input ("my
                    thrusters are slaved") if the game declares one.
T      S2 (A)       A's shot resolves as in S1; C receives Order::Damage at
                    T+1 delivered-first in its input vector.
T+1    S2 (C)       C's step: Hit; hull → 0; Destroyed emitted; wreck/
                    respawn state transitions happen immediately (R2).
                    CANNOT touch inventory here: items are ledger rows, not
                    CoreState (A2 CC-1) — the rule emits, it does not trade.
T+1    route        deliver(Destroyed) → (A, KillCredit).
T+2    S2 (A)       KillCredit lands in A's own state (own-state trace).
T+2..  [external]   The durable consequence enters as an INTENT, not an
                    event: release/spill of item X = LEDGER_ITEM_TRANSFER_OP
                    (or a game-opaque id) through gateway validation → FDB
                    transaction (sole authority) → minted receipts. The
                    anti-dupe single-ownership row is cluster-interpreted so
                    no module can bypass it (intent/mod.rs:215-257). This is
                    class boundary doing work: a domain event may ANNOUNCE
                    the spill (for presentation/witnesses); only the intent
                    transaction may EFFECT it (IV-3, A5 §5.4).
cycle-check         The receipt can later reach owner P as a persistence/
                    presentation outcome. If game logic wants P notified
                    in-rules, that notification travels as another next-tick
                    event to whoever owns a listening entity — never as a
                    callback into the transaction, never same-tick.
                    A→store→P→(event)→… stays acyclic per tick and bounded
                    per lap (§3.3).
stage-check         Every hop sits at a named boundary: admission pre-S0;
                    rules at T and T+1; credits at T+2; durable leg outside
                    the tick loop entirely (R5). No hop is "immediate"
                    across audiences; the only immediate mutations are
                    within C's own step (R2).
```

The integration lesson matches A2 §5.3's four requirements with mechanisms
now attached: visible (each hop is a named class transition), ordered (stage
labels above), owned (module-rule vs kernel seam per hop), composed-via-
declared-channels (events and intents only — no multi-entity reads anywhere
in the flow, which is also what makes every hop adjudicable emitter-side).

---

## 9. Volume bounds and overflow (proposed; owner sign-off flagged)

A4 deferred volume bounds here explicitly (G7). The external side already has
them (`MAX_OPS_PER_INTENT = 64` et al., G5); the internal side has none. What
a runaway rule could do today: emit unbounded events per tick — memory grows,
delivery queues grow, and because both authority and replay grow *identically*,
nothing detects it. Deterministic runaway is still runaway.

**Proposed (C-2), as admission bounds mirroring the intent caps:**

- **Per-entity per-tick emission cap**, kernel-checked in `step_entity` after
  the step (a constant like `MAX_EVENTS_PER_STEP`, default 64 to match
  `MAX_OPS_PER_INTENT`). Exceeding it is **not silently truncated**: truncation
  is deterministic and therefore invisible — the failure A4's P3 lesson warns
  about in another guise. Instead the tick fails loudly: a canonical error
  surfaced like a stage-1 flag (reported, shadow-counted; state remains
  last-good). Both honest parties fail identically on the same log, so
  adjudication is unaffected.
- **Delivery-queue bound** derived from the emission cap × entity cap per
  island; overflow of the *host's* queue (not a rule's emissions) is a host
  bug and asserts in debug builds.
- **Diagnostics stay uncapped but sampled** (§3.6); persistence bounds already
  exist at the envelope level (FDB 10 MB / 5 s, cited at `intent/mod.rs:184-188`).

These are proposals: constants and the fail-loud-vs-flag choice are owner
judgement, and nothing in today's tree enforces or needs them yet — no
production ruleset approaches the caps (Regolith emits single digits per
tick even in volley scenarios). Flagged to A11 with the rest.

---

## 10. Mutation log

All mutations ran against this tree; each revert re-ran the check(s) and got
the recorded passing result. Two mutations **survived** and are recorded as
findings (M-A6-2, M-A6-3), matching the standard A5 set with X2.

| # | Guarded stage broken | Named check | Observed | Reverted |
|---|---|---|---|---|
| M-A6-1 | First-writer-wins install replaced by last-writer-wins (`Entry::Vacant` → unconditional `insert` in `install_materializations`) — R3's arbitration stage | `cargo test -p orrery_core --lib executor` | `materialization_is_first_writer_wins_in_description_order` FAILED; `11 passed; 1 failed` | `12 passed` |
| M-A6-2 | Log-order fidelity: `OrderedInputs::iter()` reversed (VC-2 presentation stage) | `input_order_changes_the_outcome` **survived** — a systematic reversal keeps two orders distinguishable, so the test pins order-*sensitivity*, not log-order-*fidelity*. Second check: `cargo test -p orrery_conformance --test conformance` → `this_platform_matches_the_committed_golden` FAILED (goldens were cut under forward iteration) | survived + died, both recorded | executor `12 passed`; conformance `13 passed` |
| M-A6-3 | Delivery routing: `deliver` maps `DamageDealt` to `*attacker` instead of `*target` | `kill_credit_is_log_delivered_and_replays_from_the_killers_input` **survived** — that test hand-routes the damage input and only pins `Destroyed`'s routing target. Second check: `cargo test -p orrery_games` → `chains_match_the_committed_golden` FAILED (battery.rs:239) — no damage ever lands under misrouting | survived + died, both recorded | games suites all green (e.g. `28 passed`) |
| M-A6-4a | Coverage denominator counted per-frame-span instead of from watch advance (the exact stage witness.rs:117-127 describes as "not counted twice") | full `cargo test -p orrery_witness` | **all suites passed — survived.** The shown-ticks dedup property is documented but pinned by no named check. Finding: same class as A5 X2 (#417) | witness suites green |
| M-A6-4b | Fold-watermark duplicate gate loosened `last_tick <= folded` → `< folded` (a re-delivered frontier frame no longer short-circuits) | `cargo test -p orrery_witness --test multi_entity` | `a_multi_frame_repair_of_multi_entity_frames_closes_the_hole` FAILED at multi_entity.rs:453 ("a duplicate is ignored rather than folded twice", `frames_accepted` 8 ≠ 7); `4 passed; 1 failed` | `5 passed` |

(An earlier M-A6-4 attempt — keep-first instead of replace in
`buffer_deferred` — was discarded before recording: for byte-identical copies
it is behaviourally equivalent, so it breaks nothing and proves nothing. The
same discipline as A2's M-A′.)

Findings stated plainly:

1. **Delivery-target correctness is golden-pinned, not unit-pinned.** The
   routing table has no direct unit check; what catches a misroute is the
   committed scenario goldens, i.e. state-level evidence (M-A6-3). A cheap
   unit test asserting `deliver(DamageDealt).0 == event.target` would pin the
   clause directly; left as a suggestion, since this branch is docs-only.
2. **Log-order fidelity inside `OrderedInputs` is unpinned by unit tests**
   (M-A6-2): only the corpus catches a systematic reordering, and only
   because goldens exist. The structural code is trivially correct today;
   the finding is that nothing would notice if it stopped being.
3. **The witness coverage denominator's re-delivery immunity is unpinned**
   (M-A6-4a) — now recorded alongside #417 as an unpinned-guard inventory.

---

## 11. Stale citations found while verifying

| Record | Citation | Current truth |
|---|---|---|
| This node's issue text | "`Order::Damage` carries `from_pos` and the target adjudicates it" | Verified true; sharpened rather than stale — amount vs geometry verify at different entities (§1 note after G3) |
| A2 §9 row 7 / A1 §5.4 | facade `queue_authority_corrections` at `orrery/src/lib.rs:338` / `:336` | Definition sits at `:336` (signature) with the resource binding at `:338` — the two predecessors each cited one line of the same two-line span. Substance unaffected; recorded for precision |
| Inherited-stale set (ADR-0038 `ruleset.rs:211` drift; D21's never-implemented `validate_intent` parenthetical; docs/06:210 present-tense `classify_component` consumers; docs/10 `orrery_field_host`; brief's `p{N}-*` paths; bot.rs producer line drift) | — | Re-confirmed where touched; not re-litigated. docs/06 §3's `validate_intent`/`park_tick`/`catch_up` sketch remains designed-but-absent exactly as A1 §1.5 recorded |

No new stale citation was found in AGENTS.md or the predecessor documents'
load-bearing claims; every G-row in §1 was opened on this tree rather than
trusted.

---

## 12. Unsure, and reported rather than forced

Stated as unsure:

1. **Whether internal commands deserve their own syntax eventually.** This
   document collapses them onto events (§2.1) on replay-economy grounds; a
   future module system might want type-level distinction (command ≠ report)
   for authoring clarity. Nothing mechanical depends on it; reopening costs
   one enum wrapper at the source module.
2. **C-2's fail-loud choice for emission-cap overflow.** Flagging loudly is
   safer for detection but introduces a canonical error path that does not
   exist today (steps currently cannot fail). An alternative is treating
   overflow as a stage-1-style *flag* on an otherwise-completed tick. Both are
   deterministic; the owner should pick.
3. **Delivered-first input composition** (`scenario.rs:209-214`) is called
   "arbitrary but fixed" by its own comment. This document ratifies it as
   convention (G2) rather than necessity — but a game whose rules are
   sensitive to delivered-vs-player ordering within one tick now inherits a
   fixed convention it cannot change without a `RulesetId` bump. That is
   intended (ordering is law), yet worth the owner seeing stated.
4. **S2/S3 sequences assume lease continuity across the window.** A handoff
   mid-window adds D26/D30 machinery around the same message semantics; CC-3
   (A2) argues purity makes this seamless, and I leaned on that argument
   without building a fourth sequence to prove it end-to-end.

Reported rather than forced — decisions owned elsewhere, named not taken:

- **Rollback unit and canonical witness projection** → A7 (#403), being
  written concurrently. §7 is deliberately unit-independent; if A7 lands a
  rewind-based unit for some tier, §7's rows for that tier need revisiting
  against its record, not against this document's preferences.
- **Determinism envelope, stage semantics, gate replacement** → A4 (PR #418).
  Cited throughout; not re-specified.
- **Identity classes, P/R/W/N/A capabilities, invalid combinations** → A5
  (landed, #416). Consumed as constraints (IV-1..IV-8 are load-bearing in
  §3.4 and §8).
- **Manifest/schedule-digest format, schema-id governance, capability-
  registry construct** → A8 (#404). C-2's constants would live there.
- **Volume-bound adoption, overflow policy, and any `RulesetId` implications
  of ratifying delivered-first composition** → owner, via A11 (#407).

Deliberately not done:

- **No implementation.** All mutations lived for one command run and were
  reverted with passing results re-confirmed (§10). The only files this branch
  adds are this document and its commits.
- **No decision reserved to the owner or another node** (§12).



