# Spike: the fallout of reversing D47 clause (a)(2)

**Status: PROPOSE-ONLY, non-normative spike.** This document decides nothing,
amends no record, and implements no rollback. It exists to make one question
decidable: the owner's — *"operator rollback is a genuine feature. Spike the
fallout of reversing D47 (a)(2)."* Every decision named below is the owner's.

**Date:** 2026-08-31. **Reads from:**
[D47](../adr/0047-rollback-unit.md) clause (a),
[#809](https://github.com/baadc0de/orrery/issues/809) (the rollback contract
child), [#107](https://github.com/baadc0de/orrery/issues/107) (the P6 epic),
[docs/08](../08-persistence.md) §1, §3.4, §6, §8, §11, §14.

Every file citation was opened on this tree before it was written down. Line
numbers drift; anchor on the quoted text.

---

## 0. The verdict, in four sentences

**Operator rollback does not require reversing D47 (a)(2), and reversing it
would buy the feature nothing.** The feature the epic describes — restore a
griefed cell to a timestamp, ledger untouched — is expressible as *forward
correction*: read the pre-grief image out of the archive, apply it as an
ordinary appended journal record at the current LSN, and let the durable tier
do what it already does. D47 (a)(2) constrains where the durable tier's
*position* may point (never backwards, never an older state to a live client);
it does not constrain what *value* the latest state may hold, and a restore
changes the value while moving the position forward. The reversal is not a
smaller cost than the forward design — it is a strictly larger one, because
**the durable tier has no rewind mechanism to legalise**, and building one is
a new subsystem (per-generation checkpoints or MVCC `world/` rows) that the
feature does not need.

Three things found on the way are larger than the D47 question and are stated
in §1 before anything else, because they reorder #107.

---

## 1. Three findings that outrank the D47 question

### 1.1 There is no terrain substrate at all, and the demo criterion is built on one

The P6 demo criterion (`docs/11-roadmap.md:913`) is *"a griefer bulldozes a
player town; an operator restores it to a timestamp via archive inverse-op
replay"*, and `docs/08` §11 names the mechanism as *"terrain delta inverses"*.

Terrain is not implemented in the durable tier in any form:

- `RecordKind::TerrainDelta` is accepted on the wire (`DiffUplink.kind` doc,
  `crates/orrery_protocol/src/gateway.rs:381`, "spawn / component diff /
  despawn / terrain") and folded by **nothing**. All three fold sites match it
  to the empty arm: `crates/orrery_persistd/src/actor.rs:1409`, `:1426`, and
  `crates/orrery_persistd/src/runtime.rs:2135` each read
  `RecordKind::TerrainDelta | RecordKind::CheckpointMark => {}`.
- The `chunk/` key family has **no caller**. `keyspace::chunk_key`,
  `chunk_range_start` and `chunk_range_end`
  (`crates/orrery_persistd/src/keyspace.rs:538`, `:555`, `:571`) are referenced
  only by that module's own unit tests; a repo-wide search for them outside
  `keyspace.rs` returns nothing. The seeder says so in as many words:
  `crates/orrery_seed/src/plan.rs:98` — *"`world/` rows written (entity rows;
  v1 has no `chunk/` rows)"*.

So the object the demo restores has no journaled history, no durable row, and
no fold path — under D47 as it stands *or* reversed. **A terrain write path is
an unfiled prerequisite of #107's rollback leg**, and it is a bigger piece of
work than the contract #809 asks for. Either the demo criterion's object
changes from terrain to entities (`world/` rows, which *are* implemented end to
end), or terrain persistence gets filed and scheduled ahead of the harness.
This is an owner call and it is the first one.

### 1.2 The archive's only time axis is attacker-controlled

`docs/08` §11 specifies the tailer *"re-sorts records into `(cell_id, tick)`
order"*, and the selector §11 gives the operator is `(cell range,
author/account, time range)`. The `tick` field is client-supplied and never
validated:

- `DiffUplink`'s own doc comment lists what the server fills in
  (`crates/orrery_protocol/src/gateway.rs:366`): *"The gateway fills in the
  server-assigned `epoch`/`lsn`/`author`/`crc`"*. `tick` is not in that list —
  it is an ordinary client field at `:378`.
- The record is built by copying it verbatim:
  `crates/orrery_persistd/src/gateway.rs:8839` `let tick = diff.tick;` and
  `:8847` `tick,`. Searching `diff.tick` across the gateway returns six sites,
  all of them NACK echoes, a log field, and this copy — no range check, no
  clamp, no monotonicity test.

The griefer stamps the ticks on their own uplinks. An operator who selects
"records in time range `[T0, T1]`" therefore selects on a field the subject of
the investigation chose. This is a `risk:security-data` epic and this is the
security hole in it.

The fix is cheap and belongs to the archive-schema child, not here: **index and
select on `lsn`, which is server-assigned and monotone per node** (`Lsn` is in
the gateway-filled list above; `jarchive/{node_id}/{segment_seq}` already
records an `lsn span`, `docs/08:3235`), and carry `tick` as data rather than as
the sort key. Wall-clock across nodes comes from segment seal time. Note this
is the same divergence D47 already flagged as open in its alternatives —
*"the journal's `tick` field carries a client-local uplink sequence, not the
universe tick (A7 F-1 — its disposition is the owner's)"*
(`docs/adr/0047-rollback-unit.md`, Alternatives). This spike confirms it at the
source and shows it is load-bearing for #107's selector.

### 1.3 A diff log cannot answer "state at T", and the archive schema has to

The journal is a *partial per-component* log — `RecordKind::ComponentDiff` is
*"A component diff for an existing entity"*
(`crates/orrery_protocol/src/persist.rs:141`) and records are *"keyed by
`(entity, tick)` with **last-writer-wins per component**"* (`persist.rs:204-206`).
Reconstructing an entity's full state at time `T` therefore needs the last
write of *each component* at or before `T`, which may predate the archive's
start by any amount: an entity untouched for a year and then destroyed has no
pre-state anywhere in a 30-day archive, because its current `world/` row has
already been overwritten by the destruction.

And there is no checkpoint to fall back on. §11's *"entity state restores from
the preceding checkpoint"* has no preceding checkpoint to name:
`CheckpointStore::checkpoint` is *"Persist a checkpoint for its shard,
**overwriting any prior one**"*
(`crates/orrery_persistd/src/checkpoint/mod.rs:113`), one per shard, no
generations.

So the archive schema must be able to answer *state at T*, not merely *records
in `(T, now)`* — which means either periodic full-state records in the archive
or archived checkpoint generations. That is a requirement on the schema child
that nothing else in the epic would have surfaced, and it is where the "which
child first" ordering actually bites. Where the archive cannot answer, the
restore must **refuse that entity by name** rather than guess; a rollback that
silently invents a pre-state is worse than one that reports a hole.

---

## 2. Does operator rollback require reversing (a)(2)? No.

### 2.1 What (a)(2) actually forbids

Verbatim (`docs/adr/0047-rollback-unit.md`, clause (a)(2)):

> **Durable state is recovery-only.** The durable tier reconstructs the latest
> state (checkpoint base + journal tail replay); it never rewinds to an older
> state and never serves one to a live client

Two prohibitions, both about *position*, neither about *value*:

1. *"never rewinds to an older state"* — the recovery construction may not be
   pointed at a past position. Concretely: `ckpt/{shard}`'s watermark may not
   move backwards, the epoch fence may not move backwards, and `world/` rows
   may not be reverted to a prior generation *as a recovery act*.
2. *"never serves one to a live client"* — area load and the cold reader always
   answer with the latest.

A forward restore violates neither. The LSN it lands at is above every prior
LSN; the checkpoint watermark only advances; the epoch fence is untouched; and
what a live client is served after the restore *is* the latest state — the
restored one. The state's *value* resembles an earlier moment. The tier's
*position* never moved back. D47's own framing supports the distinction
precisely: clause (a)(1) permits canonical corrections that are *"derived by
isolated replay of the signed input log and applied **forward** as
authoritative overwrites at the current tick"* — a mechanism whose entire
purpose is to make the present hold a value derived from the past, and which
D47 nonetheless records as *"**Nothing** rewinds"*.

### 2.2 The tree already names this exact mechanism, twice, in accepted records

This is not a reading invented by this spike.

- **D11 already assigns griefing rollback to inverse-op replay.**
  `docs/adr/0011-persistence.md:19`: *"supports griefing rollback (inverse-op
  replay by cell/actor/time-range)"*.
- **D29 already reads that line as forward compensation.**
  `docs/adr/0029-low-population-path.md:512-513`: *"the inverse op is the same
  primitive D11 already names for griefing rollback (`0011-persistence.md:19`:
  \"inverse-op replay by cell/actor/time-range\")"* — and D29 clause 8
  (`:500-507`) is titled *"Annulment is a forward-written inverse, never an
  erasure"*.
- **docs/07 already specifies bulk reversal as a forward append.**
  `docs/07-witnessing.md:74` (stage 5b): *"already-journaled bulk writes are
  annulled by appending compensating inverse-op entries to the event journal"*.
- **docs/08 §11 itself says forward.** The rollback bullet ends *"and apply them
  as **administrative intents** through the critical path (audited,
  attributable)"* — an application, not a revert.

The only two places that read as a rewind are the epic's title/acceptance line
and the roadmap demo criterion. #809 already identified this and said the two
readings must not both stay in circulation. This spike agrees and adds: the
forward reading is the one three accepted records and two expansion documents
already use.

### 2.3 The decisive point: there is nothing to reverse

Suppose the owner reversed (a)(2) tomorrow. What could then be built that
cannot be built now? Nothing, because no rewind substrate exists and the
history a rewind would consume has already been deleted:

- **No point-in-time base.** One checkpoint per shard, overwritten
  (`checkpoint/mod.rs:113`). `ckpt/{grid_id}/{shard_cell_id}` is
  *"recovery watermark, and **nothing else** — the entity bag lives in `world/`
  rows only (P-8)"* (`docs/08:3234`), and `world/{grid}/{cell}/{entity}` holds
  exactly one value (`docs/08:3220`). There is no prior generation anywhere in
  the keyspace.
- **No history to replay from a past point.** Retention releases every record
  below the minimum checkpoint watermark: `release_before`'s precondition is
  *"the minimum checkpoint watermark across the shards this node hosts"*
  (`crates/orrery_persistd/src/journal/raw.rs:465-470`), the cut is executed by
  `wal.truncate_before` (`raw.rs:699-700`), and any later scan below the floor
  is a hard error, never a short answer — `guard_floor` returns
  `JournalError::Released { requested, floor }` (`raw.rs:888-893`), documented
  as *"Never a short scan: a caller that needs records below the retention
  floor is a caller whose checkpoint is older than the journal, and answering
  it with the surviving suffix would be silent data loss"*
  (`crates/orrery_persistd/src/journal/mod.rs:339-346`).

  So the deepest "rewind" the in-tree journal could ever support is **one
  checkpoint interval — 20 s, jittered** (`docs/08` §8). A griefing incident is
  minutes to hours. The journal is a redo tail, not a history.
- **Therefore the archive is the source under both readings.** And once the
  source is the archive, the write is forward under both readings too, because
  the archive is a read-only object store that cannot be "restored into"
  position — its records have to be *applied*. The reversal's only effect would
  be to license a name.

D20 makes this stronger than "unbuilt": it makes the rewind target an
*explicitly refused state*. D20 rule 4
(`docs/adr/0020-journal-retention.md:166-171`): *"`CellRuntime::open` reads from
the floor rather than from zero and refuses to open when any shard's checkpoint
watermark is below it — that combination is a checkpoint older than its own
journal, and serving it would be silent data loss."* A rewound durable state is
precisely a checkpoint older than its own journal. The runtime refuses to open
on one.

---

## 3. The forward-correction design, concretely enough to judge

Call it a **restoration record**. It is #809's question (1) answered as its
reading (a) for the bulk tier, with reading (b)'s already-built path
(D29 annulment) unchanged for anything economic.

**Inputs.** A selection: `cell ∈ CellRange`, `author ∈ {NodeId}`, `lsn ∈ [L0,
L1]` — LSN, not tick (§1.2). Plus the operator's identity.

**Step 1 — select.** Read the archive for records in the selection. This is a
read of a read-only object store by an offline tool. It is not the durable tier
serving an older state to a live client; there is no live client in this step.

**Step 2 — compute the target image.** For each entity touched in the window,
assemble its last archived component values at `lsn < L0`. Fail closed: an
entity whose pre-image the archive cannot supply (§1.3) is **named in the
refusal**, not guessed at. This step produces a plan and touches nothing.

**Step 3 — apply forward.** For each entity in the plan, emit one journal
record — a new `RecordKind::Restore` carrying the full target image is the
honest shape, since the fold is a whole-entity overwrite and not a diff — with
`author` = the operator's NodeId, at the *current* tick and the *current* LSN,
routed through the ordinary path. It goes through `apply_fenced` like every
other write: the owning cell actor's single-writer mailbox, the lease fence,
the epoch fence. Last-writer-wins per component does the rest, and the next
scheduled checkpoint folds it into `world/` with a watermark strictly above the
previous one.

**Step 4 — the ledger is not in the plan.** The restoration record may write
`world/`, `grid/` and (once §1.1 is built) `chunk/`. It may not write
`ledger/bal`, `ledger/item`, `ledger/receipt`, `intent/`, `player/`, `strike/`,
`epoch/`, `id/`, `actor/`, `ckpt/`, `lease/`, or `pid/next`. Economic damage is
not this path's business: it is D29's annulment, which is built, tested, and
already the D47 (a)(3)-compliant answer. That is exactly what "without changing
the ledger" can mean and be true.

**Step 5 — attribution comes free.** The restore *is* a journal record with the
operator's `author` and a `RecordKind` that names it, so it lands in the same
archive it was computed from. "Audited, attributable" is satisfied by the
mechanism rather than by a bolted-on log. For #615 this is the good outcome:
the conservation sweep sees a labelled bulk-tier record and no ledger delta, so
a legitimate restore is *clean, not flagged* — because it never moves value.

**Why this beats a rewind even if a rewind were free.** A durable rewind of a
cell range is a blunt instrument: it discards *innocent* concurrent state in
the same cells — the neighbour's house, built during the grief window, keyed by
the same Morton prefix. A per-entity forward restore touches only the entities
in the selection and leaves everything else at its current value. The forward
design is more precise, not merely more legal.

**Operator surface.** #809's observation holds and this spike endorses it:
`persistd` has *"no scrape endpoint and no admin surface"*
(`crates/orrery_persistd/src/gateway.rs:3082`), and the one precedent is the
watched-JSON-file drop of `--handover-request`
(`crates/orrery_persistd/src/bin/persistd.rs:219-236`), chosen precisely
*"to say 'hand shard S to node N' to a running gateway that has no admin wire
surface of its own"*, with the outcome written to `<path>.result`. A restore
request is the same shape — request in, outcome beside it — and following it
avoids inventing the first admin wire protocol as a side effect of a rollback
feature. That is a security decision and it is the owner's; this spike only
notes that the precedent exists and fits.

**What this design still owes**, and does not pretend to have: the archive
schema (§1.3), the terrain substrate (§1.1), the LSN selector (§1.2), the
account↔node binding if the selector's `author/account` half is to name a
griefer rather than a machine (#809 fact 5: `JournalRecord.author` is a
`NodeId`, `persist.rs:221-222`; `AccountId` is *"Distinct from `NodeId`"* with
the binding in `id/{account_id}`, `persist.rs:155-159`, which is #106's), and
#809's mutation check.

---

## 4. If the owner reverses (a)(2) anyway: what it costs

Priced for completeness. Each row: what breaks, record change or code change,
and size.

| # | Dependency | What breaks under a durable rewind | Kind | Size |
|---|---|---|---|---|
| 1 | `checkpoint/mod.rs` — *"the checkpoint is the base, the journal is the delta, so recovery is zero-loss by construction"* (`:3-6`) | The invariant is a two-term identity. A rewind adds a third term with no defined interaction: rewinding `world/` while `ckpt/` stays put means the tail above the watermark is *not* replayed over the rewound base, so recovery is no longer zero-loss — it is lossy by an amount nothing measures. Fixing it needs checkpoint **generations**, which `checkpoint()` explicitly does not have (*"overwriting any prior one"*, `:113`). | Code + record | **Large.** A new durable subsystem. |
| 2 | D11 §6 keyspace (`docs/08:3214-3243`) | `world/` holds one value per entity (`:3220`); `ckpt/` is *"recovery watermark, and nothing else"* (`:3234`). Neither has a version or time dimension. A rewind needs a retained prior generation on the largest, hottest row family in the cluster. | Amend D11 + edit docs/08 §6 | **Large.** Keyspace change on a permanent family; `checkpoint/mod.rs:11` says the FDB store implements it *"exactly as D11 §6 specifies"*. |
| 3 | Retention floor and the archive (D20) | Rule 4 (`0020:166-171`) *refuses to open* a runtime whose checkpoint watermark is below the journal floor — the exact shape of a rewound state. Rules 1–2 delete the pre-image the rewind would consume. A rewind window would have to become a fourth watermark in the same minimum, and rule 4's refusal narrowed. | Amend D20 | **Medium-large**, and it does not buy depth: the floor is one checkpoint interval. |
| 4 | Chain replication / followers (D23, `journal/chain_grpc.rs`) | D23 clause 1: *"The primary's retention floor travels on the chain"* (`0023:81-83`); clause 2 releases the mirror *"to the local position of the first row at or above that floor"* (`:92-93`). The follower's cursor is monotone by construction — *"the follower only ever takes the maximum"* (`chain_grpc.rs:138`) — and `rebuild_cursor` hard-fails when a batch's `predecessor != cursor.watermark` or the persisted cursor *"disagrees with the retained provenance index"* (`chain_grpc.rs:396-400`, `:424`). A primary that rewound its LSN space would re-emit `(chain, origin_lsn)` pairs the follower has already mirrored and released past. There is no reconciliation path; the chain permanently invalidates. | Amend D23 + code | **Hard mechanical blocker.** Not a cost to pay — a thing that does not work. |
| 5 | Adjudication and the witness path | **Inputs are safe; outputs are not, and this is the underestimated one.** The evidence bundle is self-contained (`t0_claim`, `t0_snapshot`, `frames`, `sibling_heads` — `adjudication.rs:560-568`), so a rewind cannot manufacture a false verdict. But adjudication's *products* are durable: `strike/{account_id}/{versionstamp}` (`docs/08:3237`) and `IntentFinality::Annulled` (`keyspace.rs:696-700`). And docs/07 stage 5b says bulk annulment is done *"by appending compensating inverse-op entries to the event journal"* (`docs/07:74`). **A durable rewind to a point before such an entry reinstates the cheat the cluster already reversed, and leaves no record that it did.** A conviction becomes un-made by an operator action taken for an unrelated reason in the same cell range. Under forward correction this cannot happen: the restore is appended *after* the annulment, the journal's own order composes them, and an auditor sees both. | Record + code | **Large, and the one to say out loud.** |
| 6 | D29 / clause (a)(3) | See §5 — it is forced, and the reason is worse than a contradiction. | — | See §5 |
| 7 | D38 at-rest schema versioning | D38's migration model is forward and lazy only. Clause (d)(4) (`0038:223-226`) pins the registry to a gapless chain *"since the oldest readable era"*, and *"a retired step leaves only after the sweep has provably passed its range"*. A rewind resurrects rows the sweep already passed, whose migrator has legally been retired, and can target an era below the oldest readable one. Clause (e) (`:246-249`) makes steps apply in *"ascending chain order"* only. | Amend D38 if the rewind may cross a schema boundary; code-only if the window is pinned to the migrator chain | **Medium.** Note the forward design has the mirror-image obligation and it is cheaper: it must *decode and re-encode at the current version*, which the self-describing formats (D38 W1, `docs/08` §16.1) already support. |
| 8 | D9 / D10 | No dependence in normative text. D9's replay anchors on a claimed snapshot, not a durable read (`0009:13`). D10's entire durable vocabulary is already *"write refusal/annulment"* (`0010:11`) — it never says "rewind", so reversal would make rewind a third, unlisted remedy rather than contradict it. | Neither (optional D9 clarification of snapshot provenance) | **Small / none.** |

**And the procedural cost.** `docs/DECISIONS.md:68-72`: *"A future decision that
changes an accepted ADR must be added as a new ADR and must name the record it
supersedes; the superseded record then changes status and links to its
replacement."* There is no implicit precedence — *"Decision numbers provide
stable references, not implicit conflict precedence."* So the reversal is a new
record naming D47, D47's status changes, and (per §4 rows 2–4, 7) D11, D20, D23
and probably D38 are amended alongside it. That is a five-record change to
enable a capability §2.3 shows would still have to be built forward.

---

## 5. Is (a)(3) forced along with (a)(2)? Yes — and worse than "contradicted"

**Loudly: a durable rewind that reaches ledger keys does not merely contradict
(a)(3). It breaks the anti-dupe invariant P5's gauntlet exists to prove.**

`ledger/bal/{account_id}/{asset_id}` is mutated by a blind atomic add and
nothing else — the annulment path writes
`trx.atomic_op(&key, &param, MutationType::Add)` with a negated delta
(`crates/orrery_persistd/src/intent/fdb.rs:1546-1550`), described there as
*"the forward-written inverse… so the reversal is arithmetically the commit's
mirror and not a recomputation of what the balance 'should' be"*. That
commutativity is the whole reason concurrent intents on one balance are safe.

A rewind writes an **absolute** value. It therefore:

- silently drops every intent that committed against that balance during the
  rewind window — including finalized, witnessed, receipted ones;
- cannot be reconciled afterwards, because `intent/{intent_id}` — the
  idempotency row — is swept at a **1 h default** (`docs/08:3228`;
  `IntentRow.gc_deadline_ms`, `keyspace.rs:715-731`). An hour after the fact
  the cluster cannot say what a committed intent did (#809 fact 3, verified),
  so a dropped intent cannot be re-applied and a client retry of one would
  **double-apply**;
- desynchronises `ledger/item/{item_uid}` from `ledger/receipt/{versionstamp}`.
  The item row is a single overwritten row and *"the single-ownership row is
  the anti-dupe invariant"* (`keyspace.rs:1704-1710`, `docs/08:3226`), while
  receipts are versionstamped and append-only. Rewinding one and not the other
  produces an item with two histories; rewinding both destroys the audit trail
  D29 clause 8 guarantees survives forever.

D29 anticipated this. `0029:527-530`: *"The journal is the event source and its
tailer may already have compacted the commit into the Parquet archive.
**Compensation appends; it does not rewrite history that has left the
cluster.**"* And `0029:305-316` refuses cascade-reversal of finalized intents
because *"there is no compensation algorithm that repairs either"*.

**So (a)(3) must be kept whatever happens to (a)(2)** — which means any
reversal has to carve the P2 key families out by name, which means the
reversal's scope is exactly the bulk families, which is exactly the scope the
forward design already covers without any reversal at all. The reversal
converges on the forward design and pays five record amendments to get there.

---

## 6. What the owner is choosing between

**Option A — keep D47 (a)(2); build operator rollback as forward correction.**
The design in §3. Amends nothing. Reuses `apply_fenced`, the lease and epoch
fences, the single-writer mailbox, and D29's annulment for anything economic.
Costs the archive schema work #107 needs regardless (§1.3), the LSN selector
fix (§1.2), a terrain substrate if the demo keeps its terrain object (§1.1),
and a new `RecordKind` plus the file-drop operator surface. **Recommended.**

**Option B — reverse (a)(2).** A new ADR naming D47, plus amendments to D11,
D20, D23 and likely D38; a new durable subsystem for checkpoint generations or
MVCC `world/` rows; and a chain-replication problem (§4 row 4) with no known
solution. Buys no rollback depth (§2.3), reinstates already-reversed cheats
(§4 row 5), and must still carve out the ledger (§5) — landing back on
Option A's scope by a longer road.

**Option C — the middle the owner might actually want.** Keep (a)(2), and open
a *named future door* in D47's own clause-(e) style for a subset-addressable
durable history, should a later feature genuinely need one. D47 already
demonstrates the pattern for exactly this situation: *"a decided position with
a named door, not a permanent prohibition"*. This spike found no requirement
that needs the door, so it names no condition — but the record's form makes it
cheap to add one later without a supersession.

**Owed records if Option A is chosen** — named here, not written:

- **#809's contract**, answering its questions (1)–(4) with §3's design. An ADR
  is the natural form; whether it is one is the owner's call, as #809 says.
- **A clarifying amendment to D11:19 and docs/08 §11**, replacing the
  ambiguous *"griefing rollback"* framing with the forward-correction reading
  D29 already applies to that same line. Not a reversal — a de-ambiguation of
  a sentence two other records already read the forward way.
- **A correction to `docs/11-roadmap.md:913`'s demo criterion**, whose "restores
  it to a timestamp" wording is the other half of the ambiguity, and whose
  terrain object may not survive §1.1.
- **D47 is not amended by any of the above.**

---

## Verification appendix

Opened on this tree before being cited. Line numbers drift; the quoted text is
the anchor.

| Claim | Source | Verified |
|---|---|---|
| (a)(2)'s exact text; the "refused, not argued case-by-case" close | `docs/adr/0047-rollback-unit.md` clause (a) | yes |
| Checkpoint overwrites any prior one; no generations | `checkpoint/mod.rs:113` | yes |
| Zero-loss = base + tail, two terms | `checkpoint/mod.rs:3-6` | yes |
| `ckpt/` is a watermark row only; the bag is in `world/` | `checkpoint/mod.rs:60-100` (`CheckpointData`), `docs/08:3234` | yes |
| Retention floor = min checkpoint watermark; caller asserts it | `journal/raw.rs:465-470` | yes |
| Physical cut via `wal.truncate_before` | `journal/raw.rs:699-700` | yes |
| Below-floor scan is a hard error, never short | `journal/raw.rs:888-893`; `journal/mod.rs:339-346` | yes |
| D20 rule 4 refuses to open on checkpoint-older-than-journal | `docs/adr/0020-journal-retention.md:166-171` | yes |
| Follower cursor monotone; rebuild fails on predecessor mismatch | `journal/chain_grpc.rs:138`, `:396-400`, `:424` | yes |
| D23 floor travels on the chain | `docs/adr/0023-follower-journal-retention.md:81-83`, `:92-93` | yes |
| Evidence bundle is self-contained | `adjudication.rs:560-568`, `:283-297` | yes |
| Annulment is a forward `Add` with negated delta + appended receipt | `intent/fdb.rs:1540-1563` | yes |
| `IntentFinality::Annulled` — "the row survives its reversal" | `keyspace.rs:696-700` | yes |
| `IntentRow` carries no ops; 1 h sweep | `keyspace.rs:715-731`; `docs/08:3228` | yes |
| Single-ownership row is the anti-dupe invariant | `keyspace.rs:1704-1710`; `docs/08:3226` | yes |
| `ProvisionalWrite` is balance-only | `intent/provisional.rs:190-200` (`PlannedWrite::ItemOwner { .. } => None`) | yes |
| Value transfer refused on the provisional path | `intent/provisional.rs:57-70`, `:120` | yes |
| `TerrainDelta` folded by nothing | `actor.rs:1409`, `:1426`; `runtime.rs:2135` | yes |
| `chunk_*` key builders have no caller | `keyspace.rs:538/555/571`; repo-wide search; `orrery_seed/src/plan.rs:98` | yes |
| `tick` is client-supplied and unvalidated | `protocol/gateway.rs:366` (server-filled list), `:378`; `persistd/gateway.rs:8839`, `:8847` | yes |
| `author` is server-assigned; it is a `NodeId`, not an `AccountId` | `protocol/gateway.rs:366`; `persist.rs:221-222`, `:155-159` | yes |
| Journal is a partial per-component diff log, LWW | `persist.rs:141`, `:204-206` | yes |
| No admin surface; `--handover-request` file drop is the precedent | `persistd/gateway.rs:3082`; `bin/persistd.rs:219-236` | yes |
| D11:19 names griefing rollback as inverse-op replay | `docs/adr/0011-persistence.md:19` | yes |
| D29:512-513 reads that line as the forward inverse primitive | `docs/adr/0029-low-population-path.md:512-513`, `:500-507`, `:527-530`, `:305-316` | yes |
| docs/07 stage 5b: bulk annulment appends to the journal | `docs/07-witnessing.md:74` | yes |
| D10's durable vocabulary is refusal/annulment only | `docs/adr/0010-witnessing.md:11` | yes |
| D38 (d)(4) gapless chain since oldest readable era; (e) ascending only | `docs/adr/0038-at-rest-schema-versioning.md:223-226`, `:246-249` | yes |
| Amendment procedure; no implicit precedence | `docs/DECISIONS.md:68-72`, `:6-8` | yes |
| §11's rollback bullet and its `(cell, author/account, time)` selector | `docs/08-persistence.md` §11 | yes |
| Demo criterion's terrain object | `docs/11-roadmap.md:913` | yes |

A read-only sub-agent was dispatched for the ADR sweep; **every quotation it
returned was subsequently re-opened and confirmed at its cited line by this
lane** before being used above, and two of its line numbers were corrected in
the process. No claim in this document rests on an unverified relay.

**One correction that sweep produced, recorded because it will trip the next
reader:** `docs/adr/0011-persistence.md` has **no numbered sections**. "D11 §6"
is a house convention for `docs/08-persistence.md` §6, the keyspace schema —
the usage `checkpoint/mod.rs:11` itself follows (*"maps the same keyspace onto
FoundationDB exactly as D11 §6 specifies"*). Under `docs/DECISIONS.md:6-8` the
ADR wins over the expansion document, so a reversal that needs the keyspace
changed has to amend D11's own prose *and* edit docs/08 §6.

**Not verified, and flagged as such:** no measurement of archive scan cost for
a `(cell range, lsn range)` selection exists, because no archive exists
(`docs/08` §11: *"the archive tailer is a P6 deliverable and released records
are not archived anywhere"*). §3's design is therefore unpriced in time, and
this spike claims nothing about how long a restore takes.
