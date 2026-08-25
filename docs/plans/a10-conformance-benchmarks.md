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
