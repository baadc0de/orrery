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
