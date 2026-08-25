# A10 — The conformance and benchmark programme (#406)

**Status:** programme proposal for the #395 planning tree · **Date:** 2026-08-25 ·
**Tree:** `docs/406-a10` (based on `main` at `2b542c4d`, which includes A8) ·
**Parents:** [#406](https://github.com/baadc0de/orrery/issues/406) ←
[#395](https://github.com/baadc0de/orrery/issues/395) · **Builds on:** all of
[A1](a1-ruleset-architecture-map.md)–[A9](a9-engine-boundaries.md) ·
**Source brief:**
[docs/plans/ruleset-ecs-migration-brief.md](ruleset-ecs-migration-brief.md)
§Testing strategy, §Phase 0/1, §Performance

This node owns the test and measurement programme: legacy-versus-candidate
differential tests, deterministic replay, worker-count variation,
persistent-data migration, rollback and authority handoff, module dependency
validation, witness fixtures, presentation extraction, and the tick, memory,
snapshot and compile-time benchmarks. Predecessor nodes fixed *formats* and
*semantics* and handed the *checks* here by name; this document turns those
hand-offs into named fixtures with measurable thresholds, and states, for
every check, what would have to break for it to fail.

Method, as in every predecessor:

- Every claim cites a file and line opened on this tree today. The four
  obligations earlier nodes handed over by name (A7 X-A, A7 X-C, A9 M3,
  A6 M-A6-4a) were each **reproduced first-hand before designing against
  them** — the mutations were re-applied, the survivals re-observed with real
  result lines, the reverts re-confirmed green (§11). Nothing below designs
  against a predecessor's memory of a gap; each gap was watched happening.
- What **exists**, what is **proposed**, and what belongs to another owner
  never share a sentence. Nothing here implements: every mutation lived one
  command run; the only file this branch adds is this document.
- Accepting or amending an ADR is the owner's; the ADR set and PR plan are
  A11's (#407). This document specifies checks and thresholds for A11 to
  sequence, and proposes; it does not decide.

---

## 0. The design rule: refuse over observe

One A3 lane ran ambiguous schedules 200/200 stable under both executors
(a3-simulation-host-comparison.md P3); `ambiguity_detection = Error` is what
mechanically rejects them. A4 §7.2 restates it; this programme is built on it:

> **Observed stability proves nothing. A conformance argument of the form
> "we ran it and it agreed" establishes luck, not a property.**

Consequences, applied throughout:

1. Every check below is classified **refuses** (a mechanism makes the bad
   state unbuildable, unloadable, or un-mergeable) or **observes** (a run is
   compared against a committed expectation). Observing checks are legitimate
   — the whole golden apparatus observes — but only when the committed
   expectation is *sensitive to the failure being claimed*. §9 is the ledger.
2. Every check names **what would have to break for it to fail**. A check
   that cannot answer that question is theatre. #417 is the worked example
   this programme must never reproduce: `feed_uplink`'s
   `LocallyAuthoritative` guard (`crates/orrery_persist_client/src/feed.rs:62`)
   can be deleted and all 95 `persist_client` tests stay green — re-verified
   on this tree today (§11 R-4) — because the "non-authoritative" fixture is
   refused by a *different* clause first. **A test that passes for a
   different reason than assumed is worse than no test, because it reads as
   coverage.** The acceptance criterion for every new fixture below is
   therefore mutation-shaped: break the guarded stage, watch the named check
   die, revert, watch it pass. A fixture ships with its kill demonstrated,
   or it does not ship.
3. Agreement runs are still recorded (A4 §6's rule: "every axis pair that
   agrees is recorded, but no agreement is ever load-bearing by itself") —
   the matrix catches what mechanisms cannot see; the mechanisms carry the
   proof.

---

## 1. Ground truth re-verified on this tree

Every claim this programme leans on was re-checked today; the four named
obligations were additionally re-executed (§11).

| # | Finding | Re-verification |
|---|---|---|
| V1 | Goldens are chains over per-tick **state hashes only**; the source names the blind spot itself ("adding attribution to `Outcome::DamageDealt` did not shift a single chain") | `crates/orrery_games/src/golden.rs:22-29`; re-proven live by reproducing X-A (§11 R-1) |
| V2 | The golden battery's tests are **macro-generated**: `chains_match_the_committed_golden` comes from `game_test!` at `crates/orrery_games/tests/battery.rs:222`; a `grep "fn chains_match"` finds nothing. Any tooling this programme adds that greps for test names must resolve macro expansion or use `cargo test -- --list` | battery.rs:222 opened today |
| V3 | Quantize-before-hash is two adjacent lines in `step_entity`: `own.quantize(); let hash = state_hash(&own);` under the comment "VC-7: snap before anything hashes or replicates it" | `crates/orrery_core/src/executor.rs:124-127`; unpinnedness re-proven by reproducing X-C (§11 R-2) |
| V4 | `feed_uplink` stamps a client-local per-entity sequence into both `tick` and `seq` (`let seq_num = seq.next.entry(entity).or_insert(0); let tick = *seq_num; … tick: Tick::new(tick), … seq: tick`) — A7's F-1, confirmed | `crates/orrery_persist_client/src/feed.rs:81-92` |
| V5 | `RulesetId` digests are placeholder constants: `[0x63; 32]` (Regolith), `[0x5C; 32]` (Skirmish); nothing in the tree computes a digest — A8's X-1, confirmed | `crates/orrery_games/src/regolith/mod.rs:76`; `crates/orrery_games/src/skirmish/mod.rs:102` |
| V6 | The witness coverage denominator counts from each watch's *advance* so "a repair re-delivering a range is not counted twice" — documented at the field, implemented at the fold | `crates/orrery_witness/src/witness.rs:117-127` (doc), `:868-886` (the `advance` computation); unpinnedness re-proven by reproducing M-A6-4a (§11 R-5) |
| V7 | The conformance corpus is five named cases (`kinematic-single`, `kinematic-swarm`, `combat-pair`, `combat-island`, `combat-isolated`), 1–16 entities, 180–600 ticks, chains committed in `corpus/golden.json`; `combat-isolated` carries the shared-vs-isolated equality across the matrix | `crates/orrery_conformance/src/corpus.rs:58-103` |
| V8 | The game battery runs four scenarios (`solo`, `duel`, `island`, `island-lossy`) for each of two games, with committed chains in `golden.rs` (`REGOLITH`, `SKIRMISH`) and a version-bump-on-regeneration rule | `crates/orrery_games/src/scenario.rs:78-99`; `src/game.rs:123`, `regolith/mod.rs:1051`, `skirmish/mod.rs:386`; golden.rs:15-18 |
| V9 | `scripts/check.sh` runs standalone workspaces from a hand-typed `WORKSPACES` table with roles: role `test` runs tests, role `check` compiles only. `gates/p2-journal-bench` carries role **check** — a bench workspace whose code compiles in CI and never executes there. New test targets outside a listed workspace run nowhere | `scripts/check.sh:90-101` (table), `:471-476` (role dispatch) |
| V10 | The P4 pipeline digest hashes exactly `orrery_witness`, `orrery_core`, `orrery_games`, `gates/p1-swarm` — **`orrery_conformance`, `orrery_persist_client` and `orrery_predict` are outside it** | `scripts/p4-ledger.sh:33-35` |
| V11 | The only numeric performance targets in the tree are D16's persistence latencies: journal commit < 2 ms server-internal, client ack p99 < 5 ms in-region, FDB < 10 ms p99 | `docs/08-persistence.md:76-77` |
| V12 | The tree's existing benches are measure-only by doctrine: persistd's `journal_latency`/`lease_renewal` (`harness = false`) assert nothing "because CI machines vary and the D16 targets … are validated by the real latency rig in a controlled environment, not a flaky unit test" | `crates/orrery_persistd/Cargo.toml:77-84`; `benches/journal_latency.rs:1-8`; standalone `gates/p4-streams-bench` (role test), `gates/p2-journal-bench` (role check) |
| V13 | `GATED_CRATES=(orrery_core orrery_games orrery_conformance)` is a typed list; `cargo tree -p orrery_witness \| grep -ci bevy` = 530 while `./scripts/core-gates.sh` exits 0 — never assume a gate covers what its name suggests | `scripts/core-gates.sh:37`; both halves re-run today: 530, exit 0 (during §11 R-3 the gate also passed *with* entity bits riding a wire payload) |
| V14 | A4's repeatability matrix (§6) specifies the workers/profile/insertion-order legs and assigns implementation here; E-M3/E-M8/E-M9 are the named mechanisms | `docs/plans/a4-deterministic-execution.md` §4, §6 |
| V15 | A7 fixed the closure format for the goldens gap — event chain + materialized-id and delivery-pair folds (G-1/G-2), four-artifact differential harness (G-3), migration round-trip fixtures (M-3), the X-C pinning test — and assigned implementation here | `docs/plans/a7-persistence-rollback-witnessing.md` §6, §7.1, §10 |
| V16 | No presentation-extraction path exists in the tree to fixture or benchmark; the brief's Phase 6 ("presentation-frame schema, AOI-limited extraction") is unbuilt | grep for extraction/presentation-frame producers in `crates/orrery/src` finds none; brief `:747-760` |

### 1.1 New findings made while verifying (not in any predecessor)

**N-1 — the scenario harness holds each `TickOutcome` only transiently; the
retained log drops event content.** A7 §6 says "the scenario harness already
holds every `TickOutcome` (`scenario.rs` `TickRecord`)". Opened today: the
loop body sees the full outcome (`scenario.rs:231-245`), but what is
*retained* is `Entry { entity, inputs, hash, state }` (`scenario.rs:119-129`)
and a bare aggregate count `events: u64` (`scenario.rs:150`, `:236`) — the
event values, materialized ids and delivery pairs are dropped at end of loop
iteration. Consequence: G-1's chain **cannot be computed post-hoc from a
`Play`**; it must be folded inside the loop (exactly where the state chain is
folded, `:235`) or the log must be widened. Either is cheap; "already holds"
just needed the precision. Fixture F-2's spec (§3) folds in-loop.

**N-2 — the P4 digest boundary is a sequencing lever this programme can
use.** V10: `orrery_conformance`, `orrery_persist_client` and `orrery_predict`
are outside the digest. So the X-C pinning test (conformance), the #417
closure fixture (persist_client), and any rollback-ladder fixture extension
(predict) can land *during* the #329 shakedown window without resetting
banked hours — while the outcome-chain fixtures (touch `orrery_games`) and
the witness re-delivery fixture (touch `orrery_witness`) are temporally
blocked until P4 exits. §10 sequences by this boundary.

**N-3 — the empty suite A9 observed under M3 is the doc-test target.**
A9 §6 recorded one suite reporting `0 passed; 0 filtered out` and flagged it
as possibly an empty compile target. Reproducing M3 today with `Running`
lines captured: `orrery_persist_client` has four populated test binaries
(95/2/2/1) and the fifth zero-line is `Doc-tests orrery_persist_client` —
an artifact of no doc-tests, not a hollow integration suite. A9's caution
("read and noted, not counted as coverage") was right; the specific worry
can be closed.

---

## 2. The four named obligations, discharged

Each obligation was handed here by a predecessor after breaking something and
watching what did *not* fail. Each was **reproduced on this tree first**
(§11), then given its closing fixture with a mutation-shaped acceptance
criterion: the fixture is accepted only when re-applying the recorded
mutation kills it *by name*, and reverting greens it.

### 2.1 A7 X-A — outcomes invisible to state-hash goldens → fixture F-2

**Reproduced (§11 R-1):** `Outcome::Expired { id: me }` appended to every
Regolith step (`regolith/mod.rs:164-166` — an event-only outcome:
`deliver` maps it to no input, `materialize` produces nothing). Result,
exactly as A7 recorded: battery `11 passed; 0 failed` including
`chains_match_the_committed_golden`; materialization `1 passed`; all of
`orrery_conformance` (13) and every `orrery_witness` suite (7/25/5/5/5/12)
green. The only kill: six hand-written event assertions in
`tests/regolith.rs` (`22 passed; 6 failed`). **Goldens certify state chains
and nothing else**; a differential harness reading goldens alone would
certify parity between implementations that disagree about what happened.

**Discharged by F-2 (§3): the committed outcome chain** — A7 G-1/G-2's
format, implemented as a second golden table beside `golden.rs`'s, folded
in-loop in the scenario harness (N-1: it must be in-loop; the retained log
drops event content). Per tick, WP-2-ordered:

```text
tick_block(t) = concat( for id in stepped order (== PersistId ascending):
                  id ‖ len(events) ‖ concat(CoreCodec(ev) for ev in events)   # emission order
                  ‖ len(materialized) ‖ concat(materialized ids)              # install order
                  ‖ len(delivered) ‖ concat((target, input) delivery pairs) ) # deliver() order
outcome_chain(t) = blake3( outcome_chain(t-1) ‖ tick_block(t) )
```

The raw material is free: `CoreEvent: CoreCodec` is already a trait bound
(`ruleset.rs:243-244` per A7), `TickOutcome` carries `events` and
`materialized` (`executor.rs:136-141`), and the delivery pairs are computed
in the loop that already calls `deliver` (`scenario.rs:237-241`). Named test
(macro-generated, V2): `outcome_chains_match_the_committed_golden` via
`game_test!`, per game, over the same four scenarios; committed tables
`REGOLITH_OUTCOMES` / `SKIRMISH_OUTCOMES` under the same
regenerate-and-bump-version rule as the state goldens (golden.rs:15-18).

**Acceptance mutation:** re-apply X-A verbatim →
`outcome_chains_match_the_committed_golden` must fail on the first tick of
every scenario while `chains_match_the_committed_golden` stays green (the
pair proves the two chains cover disjoint channels); revert → green. Second
acceptance mutation, for the delivery leg specifically: flip one `deliver`
arm to `None` (state chain unchanged until the undelivered input would have
changed state — in `island-lossy` the first divergent tick may be late or
never) → the outcome chain must move on the emission tick itself.

**What F-2 does not repeal:** adjudication still sees only state
(`ruleset.rs:280-284` doctrine); the outcome chain is a committed test
fixture, never wire, never evidence — A7 §6's boundary kept verbatim.

### 2.2 A7 X-C — quantize-before-hash unpinned → fixture F-3

**Reproduced (§11 R-2):** the two lines at `executor.rs:126-127` swapped
(hash before quantize). `cargo test -p orrery_core -p orrery_conformance
-p orrery_games -p orrery_witness`: **21 result lines, all `ok`, zero
failures — the mutation survives everything**, because every in-tree
`CoreState` stores continuous fields as lattice integers and every step
writes lattice points (`conformance/src/ruleset.rs:53-84`: "Idempotent:
`step` already wrote lattice points"). VC-7's executor snap is a live no-op;
the ordering that makes "a claim commits to exactly what replication and
persistence saw" true is pinned by no test.

**Discharged by F-3: the off-lattice pinning ruleset**, in
`orrery_conformance` (deliberately: a gated crate — the test ruleset obeys
VC-4/6/8 — and outside the P4 digest, V10/N-2, so it can land now). A
minimal `Ruleset` whose `CoreState` holds a continuous field in raw
micrometres and whose `step` deliberately writes an off-lattice value
(e.g. `pos += 1_499` µm against the 1 mm lattice); `quantize()` snaps it
per `quantize.rs`'s half-away-from-zero rule. Named test:

- `the_claimed_hash_is_of_the_quantized_state_not_the_raw_one`: drive one
  entity one tick through `Executor::step_entity`; compute
  `state_hash(quantized_expected)` and `state_hash(raw_expected)`
  independently in the test; assert the outcome's `state_hash` equals the
  former **and differs from** the latter. The inequality half is what makes
  the test refuse to pass vacuously: if the constructed state were
  accidentally on-lattice, quantized == raw and the test fails itself
  rather than silently pinning nothing (the #417 lesson applied to a
  fixture's own construction).

**Acceptance mutation:** re-apply X-C's swap → the named test dies (the
executor now hashes the raw state); revert → green. This converts X-C from
"survived every suite" to "killed by one named check", which is the entire
point of the hand-off.

### 2.3 A9 M3 — engine handles in replicated payloads → fixture F-9

**Reproduced (§11 R-3):** `entity.to_bits()` appended to the `DiffUplink`
payload in `feed_uplink` (`feed.rs:88-93` today). `cargo check` clean (after
a `bytes::Bytes` construction detail, recorded honestly in §11),
`./scripts/core-gates.sh` exit 0, `cargo test -p orrery_persist_client`:
95/2/2/1 passed, 0 failed. A Bevy `Entity` handle rides into a replicated,
journal-bound wire payload — the exact artifact A5's IV-7 forbids — and
**no named check exists**. A5's G-1, confirmed live a second time.

**Discharged by F-9: a compile-refusal fixture on the registration seam.**
The mechanism is A9's to propose and the owner's to accept (A9 §3 option 1:
a sealed `EngineHandleFree` bound on the payload-producing registration
path; option 2: registry-time schema refusal per IV-7). *Whatever* the
mechanism, the regression test this node owns is the same shape, because
A9's argument is decisive: **byte-level scanning cannot work** — entity
bits are indistinguishable from any other `u64` — so the only possible
check refuses at compile time, where the type is still a type. F-9 is a
`trybuild` (compile-fail) suite in the crate that hosts the registration
seam:

- `entity_in_replicated_payload_does_not_compile.rs`: a component type
  embedding `bevy_ecs::Entity` passed to the registration API; the
  committed `.stderr` names the unsatisfied bound.
- Companion positive case: the same component with the handle replaced by
  `PersistId` compiles — so the fixture cannot pass because the whole API
  stopped compiling.

**Acceptance mutation:** with the bound in place, re-apply M3's payload
append — it must now fail to compile at the registration site (the
mutation's kill is the compiler, and the trybuild fixture is what pins the
bound's continued existence: deleting the bound, or adding
`impl EngineHandleFree for Entity`, flips the compile-fail fixture to
"unexpected success", which is a named CI failure). Until the mechanism
lands, this gap **stays open and stays listed** — no interim byte-scanning
test will be written, because it would be exactly the false-coverage #417
warns about.

### 2.4 A6 M-A6-4a — witness shown-ticks re-delivery immunity → fixture F-8

**Reproduced (§11 R-5):** the coverage denominator's advance computation
(`witness.rs:868-886`) mutated to charge each frame's full span
(`last_tick - frame.first_tick + 1`) instead of the advance past
`newest_seen` — the exact property the field doc states ("a repair
re-delivering a range is not counted twice", `witness.rs:117-127`). Full
`cargo test -p orrery_witness`: 7/25/5/5/5/12 passed, 0 failed.
**The documented immunity has no named check at all.**

**Discharged by F-8: `a_redelivered_range_is_not_counted_twice_in_coverage`**
(in `orrery_witness`'s integration suites, beside `multi_entity.rs`'s
existing duplicate-fold test at `:453` which pins the *fold* half but not
the *counter* half — M-A6-4b died there, M-A6-4a did not). Shape: deliver a
frame spanning ticks `[a, b]`; record `shown_ticks`; re-deliver the same
range (the repair path); assert `shown_ticks` unchanged; then deliver
`[b+1, c]` and assert it advanced by exactly `c - b`. The third leg keeps
the test from passing under a mutation that stops counting entirely — a
counter frozen at zero also "never counts twice" (the both-sides-of-the-
equality trap from the brief, designed out).

**Acceptance mutation:** re-apply M-A6-4a → the middle assertion dies (span
double-counted); apply the inverse mutation (`advance = 0` always) → the
third leg dies. Revert → green. Sequencing note: `orrery_witness` is inside
the P4 digest (V10), so F-8 is temporally blocked until the #329 window
exits — recorded in §10, not silently dropped.

---

## 3. The fixture set, named

Every fixture below has a name, a home, a named check, and a §9 row saying
what breaks for it to fail. **Exists** = on `main` today; **proposed** =
this programme; homes chosen against the P4 digest boundary (N-2).

| # | Fixture | Home | Named check | Status |
|---|---|---|---|---|
| F-1 | State-chain goldens: 4 scenarios × 2 games (`golden.rs` tables) + 5-case cross-platform corpus (`corpus/golden.json`) | `orrery_games`, `orrery_conformance` | `chains_match_the_committed_golden` (battery.rs:222); `this_platform_matches_the_committed_golden` | **exists** |
| F-2 | Outcome-chain goldens: events ‖ materialized ids ‖ delivery pairs, per scenario (§2.1) | `orrery_games` (P4-blocked) | `outcome_chains_match_the_committed_golden` | proposed |
| F-3 | Off-lattice quantize pin (§2.2) | `orrery_conformance` | `the_claimed_hash_is_of_the_quantized_state_not_the_raw_one` | proposed |
| F-4 | Legacy-vs-candidate differential harness, four artifact classes (§4) | `orrery_conformance` + a standalone runner | `differential::{state,outcome,persistence,witness}_parity` per module | proposed |
| F-5 | Repeatability matrix legs: workers, profile, insertion order (§5) | conformance emit labels + CI verdict job | `projection-order-permuted` corpus case; `profile-release` and `workers-w{1,2,4}` matrix legs | proposed (platform + repeat legs **exist**) |
| F-6 | Persistence migration round-trips + refusal battery (§6.1) | `orrery_persistd` tests + committed old-format bytes | `vN_bytes_migrate_reencode_and_match_the_committed_golden`; existing `persistence_rekey_decoder_rejects_untrusted_or_stale_shapes` (X-D-proven) | partly exists (refusals live), round-trips proposed |
| F-7 | Rollback + authority handoff scenarios (§6.2); #417 closure fixture | `orrery_predict`, `orrery_persist_client` (both outside P4 digest) | `budget::tests::*` (X-E-proven, exist); proposed `an_entity_without_the_local_marker_never_feeds_the_uplink`, handoff-in-window scenario | partly exists |
| F-8 | Witness re-delivery coverage pin (§2.4) | `orrery_witness` (P4-blocked) | `a_redelivered_range_is_not_counted_twice_in_coverage` | proposed |
| F-9 | Engine-handle compile-refusal (trybuild) (§2.3) | registration-seam crate (mechanism owner: A9/owner) | `entity_in_replicated_payload_does_not_compile` | proposed, blocked on mechanism |
| F-10 | Module dependency validation battery (§7.1) | composition-root crate (Phase 2+) | `missing_dependency_refuses_composition`, `cyclic_dependency_refuses_composition`, `duplicate_schema_id_refuses_composition`, `canonical_schedule_rejects_ambiguity` (A4 E-M2 canary) | proposed, phase-gated |
| F-11 | Presentation-extraction fixture + bench (§7.2) | presentation crate (Phase 6) | `extraction_consumes_only_the_public_frame_contract` | proposed, phase-gated (V16: nothing to fixture today) |
| F-12 | Benchmark baseline manifest + suite (§8) | `gates/migration-bench` (standalone workspace, role `check`) + committed baseline JSON | harness refusal: `differential runs refuse without a committed baseline manifest` | proposed |

Golden regeneration discipline extends to every committed table this
programme adds: regenerating F-2/F-6 fixtures without bumping the producing
version (`RulesetId.version` for outcome chains, `SchemaVersion` for
migration bytes) is the same failure golden.rs:15-18 names — "a golden
regenerated without a version bump hides a rules change as a determinism
pass" — and the differential harness's classification (§4.3) depends on the
bump being honest.

---

## 4. The differential parity harness (F-4)

The brief demands: "Feed identical inputs to legacy and ECS implementations.
Compare canonical state projections, events, persistence output, and witness
hashes. Explicitly classify expected differences." A7 G-3 fixed the format —
four artifact classes, WP-2 ordering, folds as in §2.1 — and left
construction here.

### 4.1 Shape

Per migrated module (brief Phase 5 step 6), per scenario/corpus case, one
run of each implementation from identical sealed inputs (same seed, same
input log, same tick window), producing four artifacts each:

| Class | Artifact | Comparator |
|---|---|---|
| D-1 state | the per-tick state chain (F-1's fold) | byte equality of the chain |
| D-2 outcome | the outcome chain (F-2's fold: events, materialized ids, delivery pairs) | byte equality of the chain |
| D-3 persistence | the encoded at-rest bytes the run produces: framed `(ComponentTypeId, SchemaVersion, payload)` slots per WP-3, plus the journal records a `feed_uplink`-shaped producer would queue | byte equality per slot; slot set equality |
| D-4 witness | per-entity per-tick claim values (`blake3(CoreCodec(quantize(state)))`, WP-1) and the verdicts of a witness replaying each side's log against the other's claims | claim equality; both replays verdict-clean |

D-4's second half is the strongest leg and costs nothing new: the witness
pipeline already re-executes signed logs (`replay.rs:106-130` per A7 I13).
Running the *existing* adjudicator with legacy-produced claims against
candidate-produced logs (and vice versa) makes the parity argument in the
same instrument that will police production — a candidate that diverges gets
*convicted*, not just diffed.

### 4.2 Why four classes, mechanically

Each class covers failures the others cannot see, and each blindness has
been demonstrated on this tree, not asserted: D-1 alone misses event-only
outcomes (X-A, reproduced §11 R-1); D-1+D-2 miss an encoding change that
leaves semantics alone (a candidate serializing a field in a different order
produces identical chains and incompatible stored bytes — D-3's job, and
exactly what WP-3's "witness framing ≡ persistence framing" rule exists to
keep aligned); D-3 misses a projection that hashes the wrong bytes while
persisting the right ones (D-4's job — and X-C, reproduced §11 R-2, is
precisely a wrong-bytes-hashed failure that today nothing catches).

### 4.3 Expected-difference classification

Keyed by version axes, never by judgement calls, per A7 G-3 and A8's axis
table:

- equal `RulesetId.version` + equal `projection_version` (WP-6) + equal
  `SchemaVersion` set → **any difference in any class is a failure**;
- bumped `RulesetId.version` → D-1/D-2 differences become *migration
  fixtures* (committed as the new goldens with the bump, per V8's rule);
  D-3/D-4 must still match for unchanged schemas;
- bumped `SchemaVersion` on a component → D-3 differences route through the
  F-6 migration round-trip (old bytes must still load); D-1/D-2/D-4
  unchanged;
- bumped `projection_version` → D-4 claim values differ by construction;
  the harness compares each side against its own version's projection and
  refuses cross-version claim comparison (IV-2's false-deviation hazard,
  per A7 WP-6).

**The X-1 caveat, stated where it bites:** `RulesetId.digest` is a
placeholder constant today (V5) — `[0x63; 32]` and `[0x5C; 32]`, with
nothing computing them — so "equal digest" currently means "same constant",
not "same build". Until A8's digest derivation lands, the harness keys
classification off the **version fields it can trust** and treats digest
equality as carrying no information. A differential harness that leaned on
the digest would report identity between arbitrarily different builds — the
harness must not be built on that field before the field is real.

### 4.4 Refusals the harness itself makes

- **No baseline, no run** (§8.4): the harness refuses to execute a
  comparison whose "legacy" side is not pinned to a committed baseline
  manifest (commit, lockfile hash, golden versions). A comparison against
  "whatever main was this morning" is not a baseline.
- **Partial artifact set, no verdict**: all four classes or no parity claim
  — mirroring the CI verdict job's partial-matrix refusal (A4 §6).
- **Version-skew without a bump record**: if any axis differs between sides
  without a corresponding committed bump, the run fails as *unclassifiable*
  rather than sorting differences into "expected" by guesswork.

---

## 5. Deterministic replay and the repeatability matrix (F-5)

A4 §6 specified the matrix and assigned the missing legs here. The
implementation, leg by leg — existing legs cited, new legs specced:

| Axis | Values | Mechanism | Status |
|---|---|---|---|
| Platform | x86_64-linux, aarch64-linux, x86_64-windows, aarch64-macos | ci.yml determinism matrix + digest compare + partial refusal | **exists** |
| Process repeats | 10 in-process corpus repeats | nightly soak | **exists** |
| In-run repeats | identical tick ×2 | `the_same_tick_run_twice_produces_the_same_state` (executor.rs:489 area) | **exists** |
| Workers | w1 single-threaded; w2, w4 multithreaded | corpus `emit` grows a `--workers` label; verdict job compares digests across labels exactly as across platforms | proposed (E-M9); **meaningful only under a Tier-H host** — today canonical execution has no parallel executor, so the leg lands with the host, wired the day the host exists, not after |
| Build profile | dev, release | `profile-release` matrix leg: release-built corpus emit, digest compared bit-for-bit against dev's on discrete axes | proposed (E-M8); the hazard is real on this tree (A4 P-OV: `i32::MAX + 1000` panics in dev, wraps in release, because no `[profile]` override exists) — the leg must land **with** the overflow-policy decision A4 §11.4 put to the owner, else it pins today's accident |
| Insertion order | forward vs fixed permutation | new corpus case `projection-order-permuted`: same population spawned in a committed scrambled order, chain asserted equal to its forward twin (E-M3 at corpus scale) | proposed |

Two design rules inherited and enforced:

1. **Digest comparison, never "it looked stable".** Every leg emits the
   same chain digest artifact the platform matrix already emits; the verdict
   job extends its existing all-or-refuse logic. An axis that cannot emit a
   digest does not join the matrix.
2. **`projection-order-permuted` asserts nothing about naive folds** (A4
   E-M3's clause): it asserts the sorted projection's chain equality only.
   A naive-order fold agreeing across permutations would be luck; asserting
   it would enshrine luck.

Deterministic replay in the brief's sense ("repeat identical command logs
many times and compare canonical hashes") is F-1 + the soak today, and the
adjudication replay tests are its per-entity sharp edge
(`replay.rs:106-130`; `a_snapshot_that_does_not_match_its_claim_is_forgery_
not_deviation`, X-B-proven). The candidate inherits all of it unchanged —
the differential harness (§4) is what extends replay *across*
implementations rather than across runs.

---

## 6. Persistence migration, rollback, and authority handoff

### 6.1 Persistent-data migration (F-6)

The refusal half is **live and mutation-proven** — unknown component,
future version, missing step, stale rekey all refuse
(`UnregisteredComponent`/`FutureVersion`/`MissingStep`; A5 X3/X4; A7 X-D
killed `persistence_rekey_decoder_rejects_untrusted_or_stale_shapes` by
name). What does not exist is the *positive* half A7 M-3 specified:

- **Round-trip goldens:** committed old-format bag bytes (per
  `(ComponentTypeId, SchemaVersion)` slot, the `orrery_persistd/src/
  schema.rs:48-66` framing) → migrate via the registered `ComponentMigrator`
  chain → re-encode → compare against committed new-format bytes. Named
  test per step: `v{N}_bytes_migrate_reencode_and_match_the_committed_
  golden`. The committed *input* bytes are what makes this a fixture rather
  than a self-check: encode-decode-encode of current structs proves only
  that today's code agrees with itself; committed bytes from the old build
  are the other implementation in the room.
- **Downgrade/refusal symmetry:** for each supported migration, a committed
  *future*-version byte string asserted to refuse (`FutureVersion`) — so
  the fail-closed direction is pinned per-slot, not only at the machinery
  level.
- **Module-removal fixture** (brief: "module removal with persisted data
  present"): a store containing slots for a component no module declares →
  load refuses naming the component (M-2's rule). Whether an operator
  quarantine override exists is A8 manifest policy; the fixture pins the
  *default*, which is refusal.
- **Cross-version differential leg:** every schema bump gets one §4 run
  with the bump classified per §4.3 — the migration axis of the parity
  argument, distinct from the byte round-trip.

### 6.2 Rollback and authority handoff (F-7)

What exists and is proven: the budget-ladder tests
(`budget::tests::overlong_replay_evicts_enough_to_fit`,
`pathological_cost_snaps_the_own_player` — X-E killed both by name), the
adjudication replay suite (X-B), and A7's R-1 decision that the rollback
unit is the per-entity predicted set. The programme adds:

- **The #417 closure fixture**, exactly as the issue specifies: an entity
  with `Authority`/`AuthorityPhase::LocalGranted` (so every *other* clause
  in `feed_uplink` passes) but **without** `LocallyAuthoritative` — making
  the marker clause the only thing refusing it. Named test:
  `an_entity_without_the_local_marker_never_feeds_the_uplink`. Acceptance
  mutation: delete the `With<LocallyAuthoritative>` filter (reproduced
  today, §11 R-4: currently all 95 tests stay green, with the compiler's
  `unused variable: authorities` warning confirming the guard is truly
  gone) → the new test must die by name; revert → green. #417's own
  caveat carries over unreduced: whether `LocalGranted`-without-marker is
  reachable in a real race is **unsure** (§13.2); if it proves unreachable,
  the right closure is collapsing the redundant clause, not pinning an
  unreachable state — that determination belongs with the fixture PR.
- **Handoff-adjacent-to-rollback scenario** (brief: "authority handoff
  inside or adjacent to rollback"): a two-peer harness scenario in which
  authority over one entity transfers mid-window while the receiving peer
  holds ring snapshots for it — asserting (a) the ring is invalidated or
  re-anchored at the handoff tick, never replayed across it, and (b) the
  uplink guard flips exactly once (no tick where both or neither peer
  feeds). The single-writer invariant already has a two-gateway proof
  harness (`gates/p3-siblings`, commit a34839ef); this fixture is its
  prediction-tier sibling. Home: `orrery_predict`/`orrery_persist_client`
  integration (outside the P4 digest, N-2).
- **Entity creation/destruction inside the window** (brief): a scenario
  materializing and despawning an entity within the 9-tick window,
  asserting rollback of a neighbour neither resurrects the despawned
  entity nor loses the materialized one — pinning R-1's "restore is
  all-or-nothing at the entity" against the structural-change edge.

### 6.3 What is deliberately not fixtured

Canonical-state rewind: none exists anywhere in the system (A7 §2:
authorities never rewind; corrections apply forward; recovery reconstructs).
A fixture asserting "world rollback restores tick T" would pin behaviour
the architecture rejects. The rollback fixtures above test the *predictive*
mechanism and its boundaries, because that is the only rewind that is real.

---

## 7. Module validation, witness fixtures, presentation extraction

### 7.1 Module dependency validation (F-10)

Phase-gated: these checks exist the day the composition root exists
(brief Phase 2), and their defining property is that **every one refuses at
composition time** — build or startup, never mid-tick:

- `missing_dependency_refuses_composition` / `cyclic_dependency_refuses_
  composition`: a module set with an absent or circular requirement fails
  assembly with the offender named. Compile-time where composition is
  static (A8 ratified static composition), startup-refusal otherwise.
- `duplicate_schema_id_refuses_composition`: two modules declaring one
  `ComponentTypeId` refuse per A5 N-5's single-declarer rule.
- `canonical_schedule_rejects_ambiguity` (A4 E-M2, Tier H only): the real
  schedule initializes `Ok` **and** a deliberately un-ordered canary mutant
  initializes `Err`. The canary half is what keeps this from the #417
  failure mode — a passing "no ambiguity" assertion proves nothing if
  ambiguity detection was accidentally set to `Ignore`; the mutant proves
  the rejector is awake. A4 §9 E-1 prototyped both directions.
- `schedule_digest_matches_the_committed_value` (A4 §3.10): an accidental
  system reorder fails CI the way a golden does. Observes, deliberately —
  its committed expectation is exactly the thing that must not drift.
- Illegal stage registration (brief): a module registering a system into a
  stage its capabilities do not admit (e.g. a W0 module touching S5 Claim)
  refuses at registration — pinned per capability dimension the day the
  A5 registry lands.

### 7.2 Witness fixtures and presentation extraction (F-8, F-11)

Witness: F-8 (§2.4) plus what exists — detection (25), escalation,
lane-budget, multi-entity, streaming suites, and the A6-proven fold
dedup (M-A6-4b's kill at `multi_entity.rs:453`). The differential harness's
D-4 leg reuses the pipeline wholesale (§4.1). One addition beyond F-8:
**witness fixtures for the candidate must include a deliberately-lying
candidate** — a tampered candidate build re-executed under the existing
battery's tamper harness (battery.rs already runs cheat rulesets through
`adjudicate_isolated`, battery.rs:210-218) — so the programme demonstrates
the witness convicting the new implementation, not only agreeing with it.
A candidate that can only be agreed with has not been witnessed.

Presentation extraction: **nothing exists to fixture** (V16) — no
presentation-frame schema, no extraction path. The fixture and bench are
specified now so Phase 6 lands them with the feature, not after:
`extraction_consumes_only_the_public_frame_contract` (the mirror world is
built solely from emitted frames — a compile-visible property if frames are
the only export, per A9's boundary), plus the B-5 extraction benchmark
(§8.2). Recorded as phase-gated, not silently deferred.

---

## 8. The benchmark programme and the baseline (F-12)

### 8.1 The doctrine, inherited

The tree already has a measured position on benchmarks (V12): CI machines
vary, so numbers asserted in CI are flaky theatre; benches are **measure-
only** in ordinary runs, and *assertions* about performance happen against a
controlled environment (persistd's `journal_latency` header; the D16 rig).
This programme keeps that doctrine and sharpens it: benchmarks **observe**;
what **refuses** is the harness rule that a differential run without a
committed baseline is not a run (§4.4). Thresholds are evaluated in the
baseline's own environment (same host class, same pinned toolchain), as
ratios against the committed baseline — never absolute wall-clock in CI.

### 8.2 The suite

All driven from the existing instruments (corpus cases V7, scenario battery
V8), so the benchmark population is the conformance population — a number
measured on a workload no fixture covers would be a number about nothing.

| # | Benchmark | Instrument | Metric |
|---|---|---|---|
| B-1 | Tick cost | corpus cases ×(1, 16, and a new 256-entity `swarm-large` case) + scenario battery, per implementation | per-tick p50/p99 µs; µs per entity-tick |
| B-2 | Structural-change cost | a materialization-heavy scenario (Regolith `Split` storms) | per-tick cost with N installs vs 0 |
| B-3 | Memory per canonical entity | RSS delta across corpus populations; ring memory per predicted entity (A7 §13.5 names this unmeasured) | bytes/entity; bytes/predicted-entity |
| B-4 | Snapshot/journal cost | checkpoint encode of a corpus-final state; `feed_uplink`-shaped diff production; existing persistd benches for the store side | µs per snapshot; µs per diff; (store side: D16 targets stand, V11) |
| B-5 | Witness construction + presentation extraction | claim assembly per entity-tick (the `quantize+encode+blake3` path); extraction µs/frame once Phase 6 exists | µs per claim; µs per extracted frame |
| B-6 | Startup and module registration | composition-root assembly time (Phase 2+) | ms cold assemble |
| B-7 | Compile time and binary size | `cargo build --timings` clean + incremental (touch one rules file) for the workspace and for `orrery_core`/`orrery_games`; stripped binary sizes of the shipped artifacts | s clean; s incremental; bytes |

Home: a standalone `gates/migration-bench` workspace, listed in
`scripts/check.sh`'s `WORKSPACES` table with role **check** — the
`p2-journal-bench` precedent (V9): CI compiles it (so it cannot rot) and
never executes it (so it cannot flake); execution is the §8.3 procedure.
The table edit is mandatory and named here because V9's rule is absolute:
an unlisted workspace's targets run nowhere, and a bench that silently
stopped compiling is the gate-list lesson (V13) again.

### 8.3 The baseline: captured before, or it is not a baseline

**A baseline measured after migration begins is not a baseline.** The
epic's phase model puts fixtures at Phase 1 and the first behaviour move at
Phase 2; the programme makes that mechanical:

- **What already exists as a behavioural baseline, today, committed:** the
  golden chains (F-1) and corpus digests are legacy-behaviour commitments
  captured on `main` before any migration code exists. F-2's outcome chains
  extend that commitment to the state-invisible channels — which is why F-2
  must land **before Phase 2**, while the only implementation the chains
  can describe is the legacy one. An outcome golden first generated after
  composition changes would commit the candidate's behaviour as "legacy".
- **The performance baseline does not exist and is the first deliverable:**
  one run of B-1..B-7 (B-5's extraction and B-6 recorded "absent") on a
  named reference host, producing `docs/plans/baselines/a10-baseline-
  <date>.json`: every metric, plus the environment manifest — commit sha,
  `Cargo.lock` blake3, rustc version, host CPU/RAM/OS, profile flags, and
  the golden-table versions in force. Committed to the repository; the
  differential harness refuses to run without it (§4.4), which is the
  mechanism that makes "capture the baseline first" an ordering the
  programme *enforces* rather than remembers.
- **Sequencing honesty:** B-1/B-2 drive `orrery_games` scenarios but a
  baseline *run* touches no crate — only the F-2 fixture and the
  `swarm-large` corpus case are code, and their homes are split by the P4
  digest boundary (§10). The baseline can therefore be captured during the
  #329 window except for the F-2 leg, which waits with its crate.

### 8.4 Thresholds

Proposed values — measurable, and each a proposal for the owner to tighten
or loosen with the ADR set, not a decision:

| Quantity | Threshold | Rationale |
|---|---|---|
| Candidate tick cost (B-1), per case | p50 ≤ 1.10× baseline; p99 ≤ 1.25× baseline | the brief's Phase 4 exit is "performance regression is understood and accepted"; 10% median is the proposed definition of "needs no explanation", anything above it needs the written acceptance the phase demands |
| Tick budget ceiling (absolute, reference host) | p99 tick ≤ 8 ms at `swarm-large` (256 entities) | `TICK_HZ = 60` is a constant (executor.rs:25-28) → 16.6 ms frame; canonical stepping may spend at most half, leaving half for delivery, persistence feed and witness assembly — the split is proposed, the 16.6 ms is not |
| Memory per canonical entity (B-3) | ≤ 1.20× baseline | ECS archetype storage trades layout for locality; 20% is the proposed cost of admission, above it the two-world overhead risk (brief) is live and must be argued |
| Snapshot/claim path (B-4/B-5) | ≤ 1.10× baseline µs/entity | this path runs per entity per tick under witness load; it compounds |
| Store-side latency | D16 verbatim: journal < 2 ms internal, ack p99 < 5 ms, FDB < 10 ms p99 (V11) | existing accepted targets; the migration does not touch the durable tier (A7 P-1) so these must simply not move |
| Clean build (B-7) | ≤ +15% over baseline | the brief names compile time an explicit cost axis of `bevy_ecs` adoption |
| Incremental rules-crate rebuild (B-7) | ≤ +20% over baseline | the developer-loop cost the monolith complaint is partly about; regressing it while modularizing would be paying twice |
| Binary size (B-7) | ≤ +10% per shipped artifact | bevy_ecs is code the wire never sees; peers download builds under D21's three-build retention |

Threshold evaluation is a **gate on phase exit** (the Phase 4/5 acceptance
step), executed as: re-run the suite on the same host class, compare
ratios, write the comparison beside the baseline JSON. It is deliberately
not a per-commit CI assert (§8.1) — but the *presence and freshness* of the
baseline is CI-checkable and refusing (§4.4), and that is the half a
machine can hold honestly.

---

## 9. The refuse-versus-observe ledger

Every check in the programme, with what has to break for it to fail. An
"observes" row is honest only because its committed expectation was shown
sensitive to the failure class (by this node's or a predecessor's mutation);
where sensitivity is *not* yet demonstrated, the row says so.

| Check | Class | What must break for it to fail |
|---|---|---|
| core-gates clauses 1–5 (E-M1) | refuses | a banned spelling or dependency enters a **listed** crate's sources/graph (M-G1-proven). Coverage is the list (V13) — the discovery cross-check A4 §5.2 proposes is what would make unlisted escapes fail too |
| F-1 state goldens | observes | any committed per-tick state hash changes — and only that (X-A: event-only outcomes cannot fail it; that insensitivity is measured, which is why F-2 exists) |
| F-2 outcome goldens | observes | any emitted event's bytes, order, or count; any materialized id or install order; any delivery pair — X-A's mutation is the demonstrated kill |
| F-3 off-lattice pin | observes | `step_entity` hashing raw instead of quantized state (X-C's swap is the demonstrated kill); the internal `quantized ≠ raw` assertion makes vacuous passage self-failing |
| F-4 differential harness | observes + refuses | any of D-1..D-4 diverging under equal versions fails; missing baseline, partial artifacts, or unclassifiable skew **refuse to produce a verdict** at all |
| F-5 matrix legs | observes | any digest differing across platform/repeat/worker/profile/insertion cells; a missing cell refuses (verdict-job partial refusal) |
| F-6 migration round-trips | observes | migrated re-encoding differing from committed new-format bytes; refusal fixtures fail if fail-closed decoding weakens (X-D-proven for rekey; A5 X3/X4 for slots) |
| F-7 #417 closure | observes | the `With<LocallyAuthoritative>` filter alone being removed — by construction the only clause refusing that fixture's entity (pending the reachability check, §13.2) |
| F-8 re-delivery pin | observes | span double-counting (leg 2) or counting stopping entirely (leg 3) — M-A6-4a and its inverse are the two demonstrated kills |
| F-9 trybuild fixture | refuses | the `EngineHandleFree` bound (or successor mechanism) disappearing or gaining an `Entity` impl — compile succeeds where the committed expectation is failure |
| F-10 composition battery | refuses | a missing/cyclic/duplicate declaration assembling anyway; the E-M2 canary fails if ambiguity rejection is silently downgraded (the canary is the sensitivity proof) |
| Schedule digest test | observes | any reorder of stages, systems, or edges — the committed digest is definitionally sensitive to exactly that |
| F-11 extraction contract | refuses (compile-visible) | the mirror world acquiring any input other than emitted frames |
| F-12 baseline freshness | refuses | differential/benchmark comparison attempted without a committed, commit-pinned baseline manifest |
| Benchmarks B-1..B-7 | observe | thresholds (§8.4) exceeded at a phase-exit evaluation — never a per-commit assert |

Note what the table admits: most of the *new* conformance surface observes.
That is correct — parity is a comparison, and comparisons observe. The
programme's refusing spine is the composition/registration layer (F-9,
F-10, gates) plus the harness's own refusals (F-4, F-12): the places where
a bad state can be made unbuildable are all taken, and everywhere else the
committed expectation has a demonstrated kill.

---

## 10. Sequencing against the P4 digest and the workspace table

The P4 digest boundary (V10) splits the programme into what can land during
the #329 shakedown window and what must wait; the `WORKSPACES` table (V9)
determines where anything new actually executes. Neither constraint is
architectural; both are absolute while they stand.

| Item | Touches | Window-safe? |
|---|---|---|
| F-3 off-lattice pin | `orrery_conformance` | **yes** (outside digest; inside root workspace, runs in existing CI) |
| F-7 #417 closure + handoff scenarios | `orrery_persist_client`, `orrery_predict` | **yes** |
| F-12 bench workspace + baseline capture (except F-2 leg) | new `gates/migration-bench` + `scripts/check.sh` table row (role check) | **yes** (scripts/ and gates/migration-bench unhashed) |
| F-6 round-trip goldens | `orrery_persistd` | **yes** |
| `projection-order-permuted` corpus case | `orrery_conformance` | **yes** |
| F-2 outcome goldens | `orrery_games` | **no — digest reset**; first post-window PR |
| F-8 re-delivery pin | `orrery_witness` | **no — digest reset**; first post-window PR |
| `swarm-large` corpus case | `orrery_conformance` | yes |
| F-4 harness, F-5 worker/profile legs, F-9, F-10, F-11 | phase-gated on Phase 2+/Tier H/mechanism landing | n/a (post-window by construction) |

Ordering rule the sequencing must keep (A11 owns the PR plan; this is the
constraint handed to it): **F-2 lands before any Phase 2 composition PR
merges** — the outcome chains must commit legacy behaviour (§8.3), and the
window exit precedes Phase 2 in every plan variant, so the constraint is
satisfiable without touching the digest early.

---

## 11. Mutation and reproduction log

Every run today, with real result lines; baselines recorded before each
mutation; every revert re-run and green; final tree state clean
(`git status` empty of crate changes; the only addition is this document).

| # | Mutation (guarded stage broken) | Suites run | Observed | Reverted |
|---|---|---|---|---|
| R-1 (= A7 X-A) | Regolith `step` appends `Outcome::Expired { id: me }` every tick (event-only, deliver-None, no materialization) | all `orrery_games`, `orrery_conformance`, all `orrery_witness` | battery `11 passed; 0 failed` (`chains_match_the_committed_golden` green); materialization `1 passed`; conformance `13 passed`; witness 7/25/5/5/5/12 passed. Kills only in `tests/regolith.rs`: `22 passed; 6 failed` — the same six named event assertions A7 recorded | all green (11/1/28/15) |
| R-2 (= A7 X-C) | `executor.rs:126-127` swapped: hash before quantize | `orrery_core`, `orrery_conformance`, `orrery_games`, `orrery_witness` | **21 `test result: ok` lines, zero failures — survived everything**, confirming the snap is a live no-op for every in-tree state and the ordering is unpinned | core 73/15/11 green |
| R-3 (= A9 M3) | `entity.to_bits().to_le_bytes()` appended to the `DiffUplink` payload in `feed_uplink` | `./scripts/core-gates.sh`; all `orrery_persist_client` | gates exit 0; `95 passed`, `2`, `2`, `1`, doc-tests `0` — **survived; no named check exists**. Honesty note: the first mutation attempt used `extend_from_slice` on `bytes::Bytes` and did not compile — no result line was emitted, and per the brief's rule a non-compiling mutation proves nothing; rewritten via `to_vec()`/`Bytes::from` and re-run to the survival above | tests green |
| R-4 (= #417) | The `if authorities.get(diff.entity).is_err() { continue; }` guard deleted from `feed_uplink` | all `orrery_persist_client` | `95 passed; 0 failed` (+2/2/1) — survived, with rustc's `warning: unused variable: authorities` confirming the guard was genuinely gone rather than moved | tests green |
| R-5 (= A6 M-A6-4a) | `witness.rs:868-870` advance changed to full frame span (re-delivered ranges counted twice) | all `orrery_witness` | 7/25/5/5/5/12 passed, 0 failed — **survived; the documented immunity has no check** | witness + persist_client all green (final combined run) |

Steady-state re-verifications (no mutation): `cargo tree -p orrery_witness
| grep -ci bevy` → **530** with `./scripts/core-gates.sh` → exit 0 (V13,
matching A4/A7's figures); baseline `cargo test -p orrery_games` green
before R-1 began.

**Surviving mutations are findings, and all five reproductions were
survivals by design** — R-1's partial kill (six unit tests) is the measured
shape of the goldens gap, not coverage of it. None were "fixed" here;
each is closed by a named fixture in §2/§6 whose acceptance criterion is
the same mutation re-run.

---

## 12. Stale citations found while verifying

| Record | Citation / phrasing | Current truth |
|---|---|---|
| A7 §6 (G-1) | "the scenario harness already holds every `TickOutcome` (`scenario.rs` `TickRecord`, A1 §3.3); the fixture adds a fold and a committed table" | Imprecise in a way that changes the implementation: the harness *sees* each `TickOutcome` in the loop (`scenario.rs:231-245`) but **retains none of its event content** — `Entry` keeps `{entity, inputs, hash, state}` (`:119-129`) and `Play` keeps only `events: u64` (`:150`). The fold is still cheap, but it must happen in-loop; nothing can be computed from the retained log (N-1) |
| A9 §6 (M3) | "appended `entity.to_bits().to_le_bytes()` to the `DiffUplink` payload" | The payload is `bytes::Bytes`, which has no append; the mutation as literally described does not compile (R-3's first attempt emitted no result line). A9's *result* is accurate — reproduced via an equivalent construction — but anyone re-running the mutation from the text alone gets a compile error, not a survival |
| A9 §6 / §8 | one `persist_client` suite at `0 passed; 0 filtered out` flagged as possibly an empty compile target, "read and noted, not counted as coverage" | Identified today with `Running` lines captured: it is `Doc-tests orrery_persist_client` — no doc-tests, not a hollow integration binary (N-3). The caution can be retired |
| Issue #406 text | "`scripts/check.sh` carries a `WORKSPACES` table … only has its tests executed if it is listed there with role `test`" | Verified verbatim (`check.sh:90-101`, `:471-476`), and sharpened: `gates/p2-journal-bench` sits at role `check` today, so the tree already contains a bench workspace CI compiles but never runs — precedent this programme adopts deliberately (§8.2) |
| This node's briefing text | "A8 is in flight at PR #422" | True when issued; #422 merged to `main` (`2b542c4d`) before this task began writing, and this tree fast-forwarded onto it. All A8 citations here are against the merged file |
| This node's briefing text | "six hand-written event assertions in `tests/regolith.rs`" / battery/executor/feed line cites | All re-verified exact today: six named failing tests under R-1; `battery.rs:222`; `executor.rs:124-127`; `feed.rs:62`, `:81-92`; `regolith/mod.rs:76`; `skirmish/mod.rs:102`; `witness.rs:117-127` |
| Inherited stale set (A1–A8 records) | ADR-0038 drift, D21 parenthetical, docs/06:210 present tense, docs/10 `orrery_field_host` rows, `persist.rs:41-44` block-grant tense, A5 `actor.rs` ±1-line drift, A7 "DiffUplick" typo | Not re-litigated; nothing this document relies on touches them beyond what predecessors recorded |

---

## 13. Unsure

Stated as unsure rather than smoothed over:

1. **The threshold numbers in §8.4 are proposals without measurement
   behind them.** 1.10×/1.20×/8 ms are defensible starting points argued
   from the frame budget and the brief's risk register, not derived from a
   baseline that does not exist yet. The procedure is the deliverable; the
   owner should expect to revise the numbers at first capture.
2. **#417's reachability caveat is inherited intact:** whether
   `LocalGranted`-without-`LocallyAuthoritative` occurs in a real race was
   not determinable here either. The F-7 fixture is specified to force the
   marker clause to be load-bearing *in the fixture*; if the state is
   unreachable in production, the correct closure may be removing the
   redundancy instead, and that call belongs to the fixture PR with the
   authority-lifecycle evidence in hand.
3. **F-2's delivery-pair leg encodes `deliver()`'s output today**; A6 owns
   command/event semantics, and if its accepted design changes delivery
   addressing (e.g. multi-target events), the fold's pair encoding follows
   A6, not this spec. The chain's *existence* and its emission-order leg
   are insensitive to that outcome.
4. **The worker-count and profile legs cannot be demonstrated against
   today's tree** — no parallel canonical executor exists, and no
   `[profile]` policy is decided. Both legs are specified with their
   arming conditions; until then they are unexercised specification, the
   same posture A4 §11.5 took for Tier H, with the same honesty
   obligation when they first arm.
5. **The `swarm-large` (256-entity) corpus case's runtime cost** in the
   four-platform matrix is unmeasured; if it proves too slow for
   per-commit CI it should ride the nightly soak instead — placement is an
   implementation-time decision, flagged so it is decided rather than
   discovered.
6. **Whether the baseline JSON belongs in-repo or as a release artifact**
   is left open; in-repo is proposed (reviewability, the refusal check
   reads it cheaply), but a large benchmark payload may argue otherwise.

Deliberately not done:

- **No implementation.** No fixture, corpus case, bench workspace, table
  row, or gate clause was added; the five mutations lived one command run
  each and every revert was re-confirmed (§11).
- **No ADR text and no PR plan** — A11's (#407). The proposals here that
  need owner acceptance through it: the threshold table (§8.4), the
  baseline-refusal rule (§4.4/§8.3), the F-9 mechanism choice (A9's
  options), the overflow-policy coupling of the profile leg (§5), and the
  F-2-before-Phase-2 ordering constraint (§10).
