# Spike: replace P6's rollback demo criterion after the terrain deletion

**Status: PROPOSE-ONLY, non-normative spike.** This document implements
nothing, decides nothing, and amends no ADR and no accepted record. It exists
because the owner explicitly reserved one thing out of the [#830] decision:
**replacing or deferring the P6 demo criterion's bulldozed-town leg**
([D51] §Out of scope; the #830 owner comment's "owner-reserved and not decided
here"). This spike develops [terrain-substrate.md](terrain-substrate.md) §4's
three sketches into four complete candidate criteria, prices each against the
tree as it stands, states each one's dependence on D51, and recommends one.
The owner chooses; a choice made from this document is a roadmap edit the
owner makes or delegates, not something this document performs.

**Date:** 2026-09-01. **Reads from:** [docs/11](../11-roadmap.md) P6 (line 913)
and §B1, [docs/08](../08-persistence.md) §11.1, [D47], [D51],
[terrain-substrate.md](terrain-substrate.md) (#834),
[d47-durable-rewind-fallout.md](d47-durable-rewind-fallout.md) (#812), issues
[#830] and [#808], branch `feat/archive-restore-path` @ `9eca3a0`, and the
`orrery_persistd` / `orrery_games` sources cited below.

Every code citation below was opened on this tree. Line numbers drift; anchor
on the quoted shape, not the coordinate.

---

## 0. The one-paragraph version

`docs/11-roadmap.md:913`'s rollback leg demonstrates its properties *on
terrain*. D51 (Proposed) removes v1 terrain as durable state, so the leg's
object no longer exists; the leg cannot run and nothing else in the phase
would. Four candidate replacements follow. **A** (entity settlement) keeps
every property and changes only the substrate — it needs the restore path
that already exists, tested, on `feat/archive-restore-path`. **B** (annulment
gauntlet) is satisfiable from `main` today but proves only the critical-tier
half of the leg. **C** (durability without correction) is fully in-tree but
silently deletes the property the leg was the sole phase gate for. **D**
(defer) is honest and gates nothing. Recommendation: **A, with B's annulment
movement folded in as the demonstration's final act** — but B alone is
defensible if bulk restoration is judged out of P6's scope, and the choice is
the owner's.

## 1. The criterion as written, and the property underneath the fiction

The P6 demo criterion, `docs/11-roadmap.md:913`, verbatim:

> **Demo criterion.** A scripted 128-player crowd event (R6 upper bound) in
> one region: the hot cell is promoted within the hysteresis window, per-peer
> bandwidth stays within the ≤ 1 Mbps uplink budget and field-host egress
> within the ≤ 35 Mbps hot-cell budget (D6; the modeled n=128 load is ~25.6+
> Mbps, inside budget) throughout, and demotion follows dispersal cleanly.
> Then the rollback demo: a griefer bulldozes a player town; an operator
> restores it by computing its pre-grief image from the archive — selected on
> the server-assigned LSN axis, never the client-supplied tick — and applying
> it forward as attributed administrative records appended at the current
> journal position: nothing rewound, no ledger family written (the contract:
> [08-persistence.md](../08-persistence.md) §11.1). Multi-region: EU-based
> peers joining a US island get relay/gateway routing that keeps added latency
> within the measured relay penalty from P0.

This document concerns the middle leg only — *the rollback demo*. The crowd
event and multi-region legs are untouched by #830 and out of scope here.

**What the leg was actually proving.** Strip the town away and four
properties remain, each independently load-bearing elsewhere in the
architecture:

1. **History is the archive's to give.** D20 bounded the journal, so *a
   released record's history exists only where the tailer has put it*
   (roadmap:895). The leg asserts the archive can answer "what did the world
   look like before the incident" — the property P8's auditor and every
   post-P6 forensic claim also rest on (§B1's rejection of moving the
   archive out of P6, roadmap:1012).
2. **Selection on server-assigned axes only.** The gateway fills in
   `epoch`/`lsn`/`author`/`crc` (`crates/orrery_protocol/src/gateway.rs:366`);
   `tick` is client-supplied and never validated, so a griefer can stamp the
   very coordinate a time-based selection would read (#813, closed). A
   time-based operator demo is only trustworthy if it selects LSN, not tick.
3. **Correction is forward-only and attributed.** D47 (a)(2) constrains where
   the durable tier's *position* may point; §11.1 settles that a restore
   computes the target image from the archive and appends it forward at the
   current journal position — *nothing rewinds* — with the operator identity
   carried in the payload.
4. **Tier discipline.** The restore writes bulk families only and never the
   ledger (D47 (a)(3); §11.1's "what is never written"); economic damage is
   D29's annulment, and the conservation sweep (#615) must stay clean — a
   legitimate restore is clean *because it moves no value*.

The **town** contributed none of these properties. It contributed the
*object*: player-visible bulk state whose destruction a player would feel.
The fiction was that v1's player-visible bulk state is terrain. #830
established it never was; D51 deletes the pretense. The properties survive
the deletion intact — they were never terrain-specific.

## 2. How this arrived, so the decision is made with the record straight

- **#812** (the D47 spike,
  [d47-durable-rewind-fallout.md](d47-durable-rewind-fallout.md)) found while
  pricing operator rollback that there was no terrain substrate at all, and
  filed **#830**.
- **#830** verified and established: `RecordKind::TerrainDelta` folded to the
  empty arm at all three fold sites; `chunk_key` had no caller outside its own
  tests and the seeder's wipe; *the bulldozed town was not durable state*. It
  also explicitly reserved the criterion question: "Whether the demo criterion
  itself should change … is a scope decision, not an implementation one."
- **#834** ([terrain-substrate.md](terrain-substrate.md)) priced build (A:
  ~1.69 TB/day of journal ingress at 1 KiB deltas, a new checkpoint atomicity
  problem, a subsystem not a fold-arm) against delete (B: recover one clean
  prefix byte, lose nothing anyone used). Its §4 listed three demo
  consequences — entity town, different durable incident, defer — and chose
  none.
- **Owner decision, 2026-09-01, on #830: delete.** Terrain is not durable
  state in v1. The comment names the criterion "owner-reserved and not decided
  here."
- **D51** ([ADR-0051](../adr/0051-v1-terrain-is-not-durable-state.md),
  **Proposed — non-normative until accepted**) drafts that decision. The tree
  already carries its implementation: no `TerrainDelta` exists anywhere in
  `crates/`; the archive reader *refuses* the retired discriminant by name
  (`archive/object.rs:508`, "archive kind 1 was TerrainDelta and is
  permanently retired"); the keyspace registry holds the recovered `k` byte
  clean ("D51 deliberately leaves the next prefix byte clean",
  `keyspace.rs`, `registered_families`); and
  `scripts/terrain-substrate-gate.sh` (D51 §(d)) guards reintroduction.
  D51 §Out of scope: *"replacing or deferring the P6 bulldozed-town criterion
  … That demonstration cannot claim a terrain restoration once this decision
  is accepted; this record deliberately does not select its replacement."*
- §11.1 itself already bridges to this spike:
  *"Until a future terrain ADR lands, a restore covers `world/` entities only,
  which is a prerequisite of the demo criterion rather than of this
  contract"* ([docs/08](../08-persistence.md):3858-3860).

## 3. What durable, player-visible state exists today

This is the inventory any replacement criterion must draw its object from.
Two tiers, both real, both proven by in-tree tests.

### 3.1 Bulk tier — `world/`, the settlement-tier state

One ruleset component is registered durable-bulk: Regolith's `STATE`
(`crates/orrery_games/src/regolith/mod.rs:442-457` —
`PersistenceCapability::Bulk`, `RollbackCapability::Included`,
`WitnessCapability::ReplayAdjudicated`, `InterestReplicated`,
`WriteAuthorityCapability::LeaseHolder`). Its sectioned content
(`RegolithState`, `regolith/state.rs`) is what a player actually sees:

- **Craft** (`state.rs:213`): archetype, weapon, position/velocity/trail,
  hull, shield, shots fired, damage dealt, pickups won and lost,
  `score_rock_points`, kills, locks acquired, lock target/progress. A player
  flying, fighting, and scoring is writing durable rows.
- **Rock** (`state.rs:372`): tier, split generation, hull, `splits_done`,
  bloom lineage. Mining is durable.
- **Pickup** (`state.rs:409`): kind, TTL, claimant, claim age. Loot is
  durable.
- Seeded rocks carry stable `PersistId`s derived from the universe seed
  (`mod.rs:479-543`, `campaign_rock_seeds`) — content identity that survives
  every process.

The durability plumbing under it, each bit proven in-tree:

| Property | Evidence |
|---|---|
| Journaled kinds: `Spawn`/`ComponentDiff`/`Despawn`/`Rekey`/`CheckpointMark` | `orrery_protocol/src/persist.rs:140-151` |
| Fold updates actor state per entity, per-cell index, tombstones, watermarks | `orrery_persistd/src/actor.rs:1296-1426` |
| Checkpoint base written to `world/`, overwriting the prior one | `checkpoint/mod.rs:113`; `tests/checkpoint_restore.rs` |
| Recovery = checkpoint base + journal tail, concurrent, first-failure named | `tests/startup_recovery.rs` |
| Live shard handover: durable rows survive byte-for-byte, fence/epoch advance | `tests/shard_handover_fdb.rs:110` (`durable_lease_rows_and_their_cell_index_survive_a_live_shard_handover`) |
| Cold area read from the checkpoint store | `tests/area_load.rs`, `ColdCellReader` |
| Journal bounded while it happens; released records still exist in the archive | `tests/journal_retention.rs`; `tests/archive_tailer.rs` (the watermark never advances past an object not verifiably in the store) |
| Scale proof: whole-cluster `kill -9` → world resumes, zero acked intents lost, RPO 0 | P2 demo criterion, met 2026-08-21 (#239), roadmap:101 |

**The honest one-sentence summary:** a player can fly, fight, mine, loot,
score, and trade, and every bit of that survives a crash, a restart, and a
host handover today. Nothing they do to *terrain* exists at all — and per D51
that is now the design, not a gap.

### 3.2 Critical tier — intents, ledger, adjudication

- Balances, unique items (the single-ownership row *is* the anti-dupe
  invariant), receipts: `ledger/bal`/`ledger/item`/`ledger/receipt`
  (`keyspace.rs:1630-1690`).
- The P5 intent envelope: `intent/`, `attest/` co-signatures,
  `provisional/` holds — with `annul` built and tested
  (`intent/fdb.rs:1550`; D29 clause 8: each recorded write negated by a
  forward `MutationType::Add`, compensating receipt appended, replay-safe).
- Strikes: `strike/` versionstamped rows (`keyspace.rs:2264`).
- Identity: `id/` accounts/bindings, `player/` rows (`keyspace.rs:1089-1344`).
- The conservation sweep: `audit.rs` `LedgerWalk` (`:306`),
  `evaluate_pass` (`:341`); `tests/hot_ledger_sweep.rs`.

### 3.3 The archive, and the one thing not yet on this branch

The journal-to-archive tailer is landed (#808): sealed-segment consumption,
re-sort to `(grid, cell, lsn)`, verified publication before watermark advance
(`src/archive/tailer.rs`, `tests/archive_tailer.rs`; `jarchive/` metadata
`keyspace.rs:2076`).

What is **not** on this branch: `RecordKind::Restore`, the plan/apply
planner, and the operator request/outcome surface. They exist, implemented
and tested, on **`feat/archive-restore-path` @ `9eca3a0`** (unmerged): 736
lines of `archive/restore.rs` plus `tests/archive_restore.rs` (482 lines)
whose test names are the §11.1 contract in miniature —
`griefed_cell_restores_to_the_pre_grief_image`,
`restore_appends_above_the_prior_tail_without_touching_checkpoint_or_epoch_fence`,
`adversarial_ticks_cannot_move_a_grief_record_out_of_the_lsn_selection`,
`a_partial_apply_can_be_rerun_without_duplicate_entity_records`. Candidates
below price against this reality: **the restore path is a merge decision plus
a harness, not a subsystem to invent.**

## 4. Candidate replacement criteria

| | Proves | In tree today? | New work | Depends on D51? |
|---|---|---|---|---|
| **A — The entity settlement** | All four §1 properties, on bulk state | No — restore path is on `feat/archive-restore-path` | Land that branch; write the P6 harness | No |
| **B — The annulment gauntlet** | Property 4 (+ D29 compensation) only | **Yes, entirely** | Scenario harness only | No |
| **C — The settlement outlives its host** | Durability across boundaries, not correctability | **Yes, entirely** | Scenario harness only | No |
| **D — Defer behind a future terrain ADR** | Honesty only | n/a | Roadmap annotation only | **Yes — hard** |

### Candidate A — the entity settlement (forward archive restoration of bulk-state damage)

Terrain-substrate §4 option 1, developed.

**Property proved.** All four §1 properties, demonstrated on state a player
recognizes: the archive reconstructs a pre-grief image; selection is
LSN-axis (griefer-proof); correction is forward-only and attributed; the
ledger is untouched and the conservation sweep stays clean.

**Demonstration, step by step.**

1. **Fresh-seeded universe**, tailer live from genesis. The restore planner's
   pre-image rule ("last complete image before the cut", with
   `PreimageUnavailable` when the archive cannot prove coverage back to
   genesis — `restore.rs:238,256` on the restore branch) requires the archive
   to cover each candidate entity's full write history. A settlement built
   in-session satisfies this; a settlement that exists only as seeder rows
   does not (seeder rows are written to `world/` directly, never journaled —
   so **the town must be built, not seeded**; see fail modes).
2. **The settlement is built by players**: a scripted crowd assembles a
   formation of durable entities in one cell — parked cruisers with hulls and
   shields topped up, accumulated `score_rock_points`/kills, a mined rock
   pocket, dropped pickups. Every component write is journaled and archived.
3. **Enough history accrues** for the tailer to seal and verify ≥ 2 segments
   and for journal release to clamp behind the archive watermark
   (`archive_tailer.rs` behavior) — the demo runs *with retention active*, or
   it is not the configuration the deployment runs (the P2 criterion's own
   clause).
4. **The grief**: a scripted client on a named `NodeId` destroys and vandalizes
   the settlement — hulls to zero (wrecks → `Despawn`), survivors' components
   damaged. Every record carries the gateway-filled `author`
   (`gateway.rs:366`).
5. **The operator request**: a JSON file dropped for the node to watch — the
   exact shape §11.1 prescribes, following the `--handover-request` precedent
   (`bin/persistd.rs:2054`; `bin/persistd.rs` watches, renames aside, writes
   `<path>.result`). Selection: source node, grid, cell range around the
   settlement, `[L0, L1]` mapped from segment seal metadata, `author` =
   the griefer's `NodeId`. The operator identity is named in the request.
6. **The plan is inspected before applying** and must contain, per candidate:
   `Restorable`, and at least one **`Refused(PreimageUnavailable)`** and one
   **`Held`** (an adjudication product inside the window) — the fail-closed
   paths are part of the demonstration, not exceptions to it. A demo whose
   plan is uniformly restorable is a happy-path demo (see fail modes).
7. **The apply**: one `RecordKind::Restore` per restorable entity, appended at
   the *current* LSN through the owning actor's ordinary serialization.
   Destroyed entities reappear with their pre-grief images; damaged
   components revert. The `.result` file records every disposition.
8. **Assertions, all machine-checkable**:
   - restored images byte-equal to the archive-derived targets;
   - LSN and tick strictly monotonic across the whole incident — nothing
     rewound (`ckpt/` watermark and epoch fence untouched);
   - zero rows written in any family outside the bulk checkpoint families —
     the keyspace registry (`registered_families`) and the ambient FDB audit
     are the enforcement surface;
   - the conservation sweep runs clean before and after (#615's
     `LedgerWalk`): the restore moved no value;
   - a re-run of the same `plan_id` applies nothing twice (partial-apply
     idempotence, tested on the restore branch).

**Already exists.** The tailer and its clamp; `world/` folds, checkpoints,
recovery, handover; the author axis; `jarchive/` metadata; the audit sweep;
the §11.1 contract prose; and the entire restore path with the four named
tests above, on `feat/archive-restore-path`.

**Must be built.** (i) Land the restore branch — review and merge, not
authorship. (ii) The P6 harness: settlement-building bots, the griefer bot,
the operator request/response wiring, the assertions — a `gates/`-style
scenario, in the lineage of `gates/p2-load`. (iii) The adjudication-product
wiring if the `Held` path needs a real verdict inside the window (a strike
against one candidate from the P4/P5 machinery suffices).

**How it could fail to be convincing.**
- *The re-seed trap*: "restoring" by re-running the seeder proves nothing
  about the archive. Mutation check: corrupt or remove one archived object;
  the plan must refuse the affected entities by name (`PreimageUnavailable`)
  rather than fall back to seed content.
- *Vacuous pre-images*: entities whose components were never written inside
  the archive's coverage are refused — correct behavior, but a demo that
  accidentally only exercises refused candidates proves nothing either. The
  plan must show a substantial restorable majority.
- *Whole-world identity overreach*: the simulation continues post-incident
  (trails sample, cooldowns tick, TTLs expire), so a whole-world hash cannot
  be equal. The assertion must be scoped to the restored components' values —
  exactly the pre-images — or a passing test would be false and a false one
  would be discovered only later.
- *The `Held` path window-dressed*: if no adjudication product actually
  touches a candidate, the hold assertion is vacuous. The window must contain
  one real adjudicated event.
- *Scope creep*: this leg proves restoration, not the crowd-event budgets or
  multi-region latency. It must not be allowed to silently stand in for legs
  1 and 3.

**Depends on D51?** **No.** A is valid whether D51 is accepted (terrain gone;
bulk restoration is the only restoration there is) or rejected and terrain
later built (A remains the entity-tier leg; a terrain leg would be *additional*,
needing its own ADR per D51 §(a)). It is *motivated* by D51 but not *gated* on
it.

### Candidate B — the annulment gauntlet (economic grief corrected compensation-only)

**Property proved.** §1 property 4, in its strongest form: the durable tier's
response to malice on critical state is **compensation, never restoration**
(D47 (a)(3)); the ledger remains conserved and machine-checkably audited
through a real incident; strikes attach; the "no ledger family written"
clause has a real enforcement surface rather than being a negative assertion
about a path that doesn't exist.

**Demonstration, step by step.**

1. Two accounts hold seeded ledger rows; a legitimate trade completes through
   the P5 intent flow (read-check-write in one FDB serializable transaction).
2. The griefer attempts the P5 gauntlet — replayed intent (idempotency
   rejection), double-spend race (one FDB conflict + failed validation),
   forged attestation — all refused with audit trails. (These are P5's
   (a)–(d); they are the *setup*, not the new content.)
3. **One grief succeeds before detection**: a committed intent that
   adjudication later convicts (tamper evidence via the P4 replay path, or
   witness deviation verdict). This is the leg's essential difference from
   P5: something bad actually commits.
4. Adjudication renders the verdict; the cluster **annuls** the guilty intent
   (`intent/fdb.rs:1550`): each recorded write negated by a forward
   compensating mutation, a compensating receipt appended, the intent row
   marked `Annulled` (`keyspace.rs` `IntentFinality`, `:645-647`), a strike
   row written.
5. The conservation sweep (`audit.rs` `LedgerWalk`/`evaluate_pass`) runs
   before and after: zero unconserved value; the compensating entries
   reconcile to the last receipt; the sweep's finding log is empty.
6. Every refusal, verdict, and annulment is machine-checkable from the
   journal/ledger rows themselves — attribution travels with the records.

**Already exists.** Everything: the intent envelope, idempotency, attestation,
annulment, strikes, the audit sweep — with tests (`intent_commit.rs`,
`intent_stage_decomposition.rs`, `hot_ledger_sweep.rs`,
`report_escalation.rs`). **This candidate is satisfiable from this branch
today**, with harness work only.

**Must be built.** A scenario harness: bots, the operator/adjudicator script,
the assertions. Possibly gluing the adjudication verdict to the annul trigger
inside the harness if the demo wants one continuous story rather than two
scripted steps.

**How it could fail to be convincing.**
- *The P5 re-run*: legs (a)–(d) of P5's criterion (roadmap:882) already
  prove the refusals. B's only new content is the **post-commit annulment +
  sweep** movement. A demo dominated by refusals proves what P5 already
  proved.
- *Narrower than the original*: it proves nothing about bulk/`world/` state
  or the archive; a destroyed settlement stays destroyed. As the *sole*
  rollback leg it silently deletes §1 properties 1–3 from the roadmap.
- *The staged failure*: the "successful grief" must genuinely commit and be
  genuinely adjudicated; a scripted refusal wearing annulment's clothes is
  theater the audit trail itself would expose.

**Depends on D51?** **No — not at all.** This is the candidate to pick if the
owner wants a criterion runnable the day D51 (or any replacement wording)
lands.

### Candidate C — the settlement outlives its host (durability without correction)

**Property proved.** Player-visible bulk state survives every continuity
boundary the system actually has — live shard handover mid-play, full-cluster
`kill -9`, restart — and durable truth *includes* the grief: the damage
persists because it happened. This is the Destiny-2 lesson the P6 goal names
(move the host into the datacenter), demonstrated on state a player
recognizes.

**Demonstration, step by step.**

1. The crowd builds the settlement; a checkpoint lands; the griefer damages
   part of it. **No rollback** — the damage persisting is the point.
2. Mid-play, `kill -9` the node owning the settlement's cell; the shard
   hands to another node (`shard_handover` path); peers keep playing.
3. Then `kill -9` the entire cluster; restart; recovery loads checkpoints and
   replays journal tails (`startup_recovery.rs`).
4. Assertions: surviving and damaged entities byte-identical across both
   boundaries; zero acked intents lost; both journals released behind their
   floors during the run (the P2 criterion's bounded-journal clause);
   fence/epoch continuity through the handover.

**Already exists.** All of it — `checkpoint_restore.rs`,
`startup_recovery.rs`, `shard_handover{,_fdb,_gateway}.rs`,
`journal_retention.rs`, and P2's met criterion (roadmap:101) as the scale
version.

**Must be built.** A scenario harness only.

**How it could fail to be convincing.**
- *Redundancy*: P2 already demonstrated kill -9 → world resumes at 10k
  entities. Unless the handover-mid-incident and the player-visible framing
  carry real new weight, this is a re-measurement, not a criterion.
- *It drops the property*: operator correction disappears from the roadmap
  entirely — nothing in P0–P7 would test §11.1's contract end-to-end, and
  §1 property 1 (the archive answers history questions) loses its only phase
  gate.
- *Reads as a regression*: "griefing rollback" is a named persistd
  responsibility at roadmap:890; a criterion whose answer to griefing is
  "it persists" collides with that line until the owner rewords both together.

**Depends on D51?** **No.**

### Candidate D — defer the leg behind a future terrain substrate

Terrain-substrate §4 option 3.

**Property proved.** Honesty only: no criterion claims a demonstration the
tree cannot run. The P6 criterion is annotated as blocked on a future terrain
ADR — which, per D51 §(a), cannot restore the removed names as placeholders
and needs a new owner decision defining payload, replay, checkpoint atomicity,
admission, archive semantics, and a new key allocation.

**Demonstration.** None; the leg is marked deferred and P6 acceptance is
scoped to the crowd-event and multi-region legs.

**Already exists / must be built.** Documentation only.

**How it could fail to be convincing.**
- *It gates nothing*: the roadmap's own definition (AGENTS.md, citing D17) is
  that each phase's demo criterion is a **permanent regression harness** that
  gates entry to the next phase. A leg that cannot run is not a harness, and
  P6 exit would be contingent on unbuilt, unaccepted work — the exact
  shape #830 called indefensible when the criterion *silently* depended on
  missing substrate.
- *The properties lose their only gate*: §1 properties 1–4 would have no
  phase demonstration anywhere in P0–P6.
- *Precedent*: the first criterion explicitly allowed to pend future work is
  the argument for the second.

**Depends on D51?** **Yes — hard.** Deferral presumes D51 is accepted (terrain
not durable in v1) and only makes sense if a terrain return is plausibly
scheduled. If the owner rejects D51 and terrain is built, the original leg
becomes runnable and deferral is moot. Note the inversion: D is the only
candidate whose validity tracks D51's acceptance; A/B/C are indifferent to it.

## 5. Recommendation — advice, not decision

**Recommend A, with B's annulment movement folded in as the demonstration's
final act.** Concretely: after the restore applies and the assertions hold,
the demo's incident includes one committed-then-adjudicated economic touch —
annulled, compensating receipt appended — and the conservation sweep runs
clean over the whole aftermath. That folding exists because A's "no ledger
family written" clause is a *negative* assertion: it is only meaningful if
something economic could have been touched, and B supplies exactly that with
machinery that is already built.

Why A as the spine:

1. **It preserves the property and changes only the substrate.** The leg's
   four properties (§1) all survive translation; the fiction was the town's
   material, not the mechanism. The criterion keeps demonstrating the thing
   it was always about: the archive is the source of pre-incident truth,
   selection is griefer-proof, correction is forward and attributed, and the
   tiers hold.
2. **The landed contract already anticipated exactly this.** §11.1 ends by
   saying a restore "covers `world/` entities only, which is a prerequisite
   of the demo criterion rather than of this contract"
   ([docs/08](../08-persistence.md):3858-3860). A makes the criterion and the
   contract agree.
3. **The delta is a merge decision plus a harness.** The restore path exists
   and is tested against the contract's hardest clauses (tick-proof
   selection, no checkpoint/fence touch, partial-apply idempotence) on
   `feat/archive-restore-path`. No candidate other than A exercises that
   branch's work; choosing B or C leaves it unmerged and unjudged by any
   phase gate.

Why not the others *alone*: B is the cheapest honest option and is defensible
if the owner judges bulk-state restoration out of P6's scope — but it
silently deletes §1 properties 1–3 from the roadmap. C is fully in-tree but
proves durability, not correctability, and partially duplicates P2's met
criterion. D gates nothing and is the only candidate that hard-depends on D51.

**The choice is the owner's.** The decision this document serves is a scope
decision about what P6 must prove — exactly the decision #830's owner comment
reserved, and D51 §Out of scope deliberately does not make.

## 6. Owner checklist

1. **Pick the replacement**: A, B, C, D, or A+B as recommended.
2. **Accept or reject D51 first or simultaneously.** A/B/C do not require it,
   but the *wording* of any replacement should not cite terrain; D requires
   it.
3. **Coordinate the adjacent text** — flagged here, not decided:
   - roadmap:890 names `terrain chunk compaction` and `griefing rollback`
     among persistd's P6 duties; the first is stranded by D51, the second is
     the phrase a non-restoration candidate (B/C) would need to reword.
   - roadmap:894 is a full deliverable ("Terrain pipeline: cell-aligned
     chunk deltas … compacted to ≤ 100 KB snapshot shards") with no substrate
     under it — the same treatment the criterion gets should reach it.
   - §B1's rejection of moving the terrain pipeline out (roadmap:1010-1015)
     rests on "the rollback demo exercises it"; that reason dissolves under
     any candidate here, and §B1 (PROPOSED in its entirety) would deserve a
     matching note when the criterion is rewritten.
   - The P2 section already carries D51 annotations (roadmap:95, :97); the
     P6 section is the remaining un-annotated terrain text.
4. **The roadmap edit itself** — a criterion rewrite in an accepted expansion
   document — should follow whichever ADR/recording convention the owner
   prefers for criterion changes; this spike deliberately performs none of
   it.

## 7. Verification appendix

Checked on this tree (branch `docs/p6-demo-criterion`):

- The criterion quote in §1 is verbatim from `docs/11-roadmap.md:913`.
- `RecordKind` has exactly five variants, no `Restore`, no `TerrainDelta`
  (`orrery_protocol/src/persist.rs:140-151`); the fold sites are the
  Spawn/ComponentDiff/Despawn/Rekey/CheckpointMark arms at
  `actor.rs:1296-1426` — the three empty-terrain-arm sites #830 named no
  longer exist.
- The `k` family is absent from `registered_families` (`keyspace.rs:3656 ff.`,
  with the "D51 deliberately leaves the next prefix byte clean" comment in
  place); `archive/object.rs:508` refuses archive discriminant 1 by name;
  `scripts/terrain-substrate-gate.sh` exists.
- `RecordKind::Restore` and the planner/apply surface are **not** on this
  branch; all restore-branch citations were read via
  `git show 9eca3a0:...` (`feat/archive-restore-path`, unmerged).
- The tailer, annul, audit-sweep, handover, and recovery claims were verified
  against the named files and tests on this branch.
- D51's status is **Proposed** ([DECISIONS.md](../DECISIONS.md):93-97: "It is
  deliberately absent from the index above until the owner accepts it");
  #830 is CLOSED with the owner's 2026-09-01 decision comment; #808 (the
  tailer) is CLOSED/landed; #813 (the tick axis) is CLOSED.

[#830]: https://github.com/baadc0de/orrery/issues/830
[#808]: https://github.com/baadc0de/orrery/issues/808
[D47]: ../adr/0047-rollback-unit.md
[D51]: ../adr/0051-v1-terrain-is-not-durable-state.md
