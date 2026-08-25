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
