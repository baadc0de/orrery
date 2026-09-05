# A18 stage audit — what is actually open on `origin/main`, 2026-09-05

> Audit of [A18](a18-ruleset-ecs-implementation-programme.md) (#625) and its
> epic [#626](https://github.com/baadc0de/orrery/issues/626) against
> `origin/main` at **`23c6e41`**. Every `path:line` below was opened at that
> commit before being written down; where A18's own citation no longer points
> at what it claims, section 4 gives the correct one. **Read-only on code:**
> this node changes nothing but itself, and it does not amend A18, which is a
> dated record of 2026-08-28.
>
> Occasion: two stages have now been dispatched as "open" and found already
> merged (S0 in #643/#818/#629, S2 in `scripts/core-gates.sh`). A18 warned in
> its own header that this corpus drifts hard. It does; A18 is now the one
> that has drifted.

## 1. Verdict up front

**Fourteen of the sixteen filed lanes are built and merged. Two things are
genuinely open, and one of them is invisible from the plan and from the ADRs
alike.**

The epic's child table reports **16/16 sub-issues completed**, and against the
code that count is honest for fourteen lanes. What it hides is a category the
issue tracker cannot represent: a lane closed on a *decision* whose *condition*
was never built. S0.b is exactly that, and it is not the only instance of the
shape — three ADR clauses are now stale in the *opposite* direction, still
telling a reader that a mechanism does not exist when it landed days ago.

What is actually open:

1. **S0.b's second half** — the undefaulted `Ruleset` obligation D43's
   2026-08-31 amendment requires. Not on main; in flight as PR #1099.
2. **S6, all three lanes** — driver convergence. Its entry condition (S5 exit)
   has been met since `orrery_sim_host` landed, and **no child issues were ever
   filed**, so the stage has no tracked existence at all.

Everything else in S0–S5 is on main with a named check behind it. S7 has run
well past what A18 describes. **S1 is complete** — reported here as fact only,
per the owner's 2026-08-29 standing rule that goldens are not a priority; it is
not ranked and no S1 work is recommended.

## 2. Stage-by-stage

| Stage / lane | State | Evidence on `23c6e41` |
|---|---|---|
| **S0.a** profile pin | **Done** | `Cargo.toml:208-212` `[profile.dev]` / `[profile.release]` `overflow-checks = false`, with the clause reasoning at `:196-207`. PR #643 (`cb2a4c5`). The mutation-killable half landed too, as a *behavioural* test rather than a manifest parse: `crates/orrery_core/src/lib.rs:132` `canonical_arithmetic_wraps_instead_of_panicking`. **Not** at A18's proposed path — see §4 |
| **S0.b** overflow scoping | **Partly done — the load-bearing half is missing** | Decision half: `docs/adr/0043-…:551` "Amended 2026-08-31 (owner-authorised), per [#628] … The owner chose **(B) scoping** over **(A) parity**". Condition half: that amendment requires, at `:557-558`, "a `Ruleset` trait obligation with **no default** — for example, an associated constant `const OVERFLOW_IS_CANONICAL: bool;`". **`OVERFLOW_IS_CANONICAL` occurs exactly once in the entire tree, in `docs/adr/0043-…:558`, the sentence that specifies it.** The trait at `crates/orrery_core/src/ruleset.rs:294-393` carries `id`, `max_neighbor_reads`, `max_neighbor_staleness_ticks`, `step`, `materialize`, `invariants` and nothing else. PR #1099 is OPEN, unmerged. §3.1 |
| **S0.c** D21 footnote | **Done** | `docs/adr/0021-ruleset-distribution.md:26` "**Correction, 2026-08-30 (#629).**" — the parenthetical at `:20-21` is left standing and dated beneath, which is the right shape for an ADR |
| **S1.a** F-2 outcome chains | **Done** (not ranked) | `crates/orrery_games/src/golden.rs:87` `REGOLITH_OUTCOMES`, `:180` `SKIRMISH_OUTCOMES`, plus `:243` `REGOLITH_WORLD_OUTCOMES` added later. The in-loop fold is `crates/orrery_games/src/scenario.rs:476` `fold_outcome_tick` into `:503 outcome_chain`. Both named checks exist and are distinct: `crates/orrery_games/tests/battery.rs:254` `chains_match_the_committed_golden` and `:282` `outcome_chains_match_the_committed_golden` — the survivor/kill pair A18's detector asked for |
| **S1.b** F-8 witness pin | **Done** (not ranked) | `crates/orrery_witness/tests/detection.rs:997` `a_redelivered_range_is_not_counted_twice_in_coverage`, carrying all three legs by name in the source: leg 2 at `:1039-1047` ("re-deliver unchanged … advances coverage by zero"), leg 3 at `:1104-1111` ("extend: an overlapping range advances coverage by only its new span") |
| **S1.c** F-5 corpus legs | **Done** (not ranked) | `crates/orrery_conformance/src/corpus.rs:103` `projection-order-permuted`, `:114` `swarm-large`; the committed permutation at `:66` and its differential twin assertion at `:563-567` |
| **S1.d** F-6 migration goldens | **Done** (not ranked) | Committed old-format bytes now exist: `crates/orrery_persistd/goldens/component-17-v{0,1,2}.postcard.hex`, `component-18-v0.postcard.hex`, `README.md`, included at `crates/orrery_persistd/src/migration.rs:474-478` with provenance noted at `:483`. Module-removal refusal at `:578` `removed_module_bytes_refuse_naming_the_persisted_component` |
| **S1.e** F-12 bench workspace | **Done** (not ranked) | `gates/migration-bench/` (`build.rs`, `src/{baseline,capacity,environment,report,suite,main}.rs`); the `check`-role row at `scripts/check.sh:104` `'gates/migration-bench  check'`; first baseline committed at `docs/plans/baselines/a18-baseline-2026-08-30.json`. The refusal rule is real and field-named: `gates/migration-bench/src/environment.rs:8` "the differential harness refuses, by named field, to compare across" |
| **S2** Tier V role discovery | **Done** | `scripts/core-gates.sh:175` `discover_role_crates`, declared floor at `:54` `DECLARED_GATED_CRATES`, union at `:206-210`, `cfg(test)` stripping at `:110` `strip_cfg_test_items` (with the persistd test-impl rationale in its header at `:105-109`), and D43(d)(3)'s deliberately-broader second source at `:183-189` `impl_bearing_crates`. Staleness check on the declared floor at `:361-369` |
| **S3.a** OD-21 `DiffUplink.tick` | **Done** — the code half was taken | `crates/orrery_persist_client/src/feed.rs:60` `universe_tick: Res<ContactTick>` and `:90` `tick: universe_tick.tick`, with `seq` now a separate counter at `:82-93`. The doc it had to agree with is unchanged and is now true: `crates/orrery_protocol/src/gateway.rs:378` "The universe tick at append (D8)" |
| **S3.b** F-7 rollback scenarios | **Done** | Both scenarios have their own files: `crates/orrery_predict/tests/handoff_window.rs` and `crates/orrery_predict/tests/creation_destruction_window.rs` |
| **S3.c** OD-26 / IV-7 | **Done in code; the record still denies it** | The compile-time mechanism exists and bites at the registration call site: sealed `EngineHandleFree` at `crates/orrery_replicon/src/lib.rs:47`, carried into the public seam at `:426-430` (`replicate<C>`) and `:436-438` (`replicate_diff<C>`), enforced through the guarded rule impl at `:285-288`. The trybuild suite is committed with both twins: `crates/orrery_replicon/tests/ui/entity_in_replicated_payload_does_not_compile.rs` + its `.stderr` ("the trait `EngineHandleFree` is not implemented for `bevy_ecs::entity::Entity`") and the positive `engine_handle_free_payload_compiles.rs`. **But `docs/adr/0045-…:242` still reads "no mechanism exists today", and `:380-381` still says "Until at least one lands, IV-7 is review-held".** §3.2 |
| **S3.d** X-1 digest | **Done — placeholder retired** | `crates/orrery_games/src/regolith/mod.rs:434-440` `REGOLITH_RULESET` now takes `digest: crate::ruleset_digest::RULESET_DIGEST`; same at `crates/orrery_games/src/skirmish/mod.rs:131`. Generated by build script: `crates/orrery_games/build.rs:2` calls `orrery_ruleset_digest::generate_build_output` (`crates/orrery_ruleset_digest/src/lib.rs:154`, with a verified manifest closure and `rerun-if-changed` over its inputs at `:157-161`), included at `crates/orrery_games/src/lib.rs:67-68`. `[0x66; 32]` is gone. **`docs/adr/0049-…:454` still says "the digest remains the placeholder Context §1 describes".** §3.2 |
| **S4.1** composition root | **Done** | New crate `crates/orrery_compose` — the plain struct-of-tables manifest at `src/lib.rs:234` `ComponentSchemaManifest` and `:245` `ModuleManifest`; the reviewed `ComponentTypeId` registry at `crates/orrery_compose/src/registry.rs`, consumed rather than duplicated (`crates/orrery_games/src/regolith/mod.rs:635`). F-10's three refusals exist by name: `crates/orrery_compose/src/lib.rs:677` `missing_dependency_refuses_composition`, `:693` `cyclic_dependency_refuses_composition`, `:712` `duplicate_schema_id_refuses_composition` |
| **S4.2** two Regolith domains | **Done** | Two delegated modules with their own files and their own headers: `crates/orrery_games/src/regolith/craft.rs:1` "The `regolith.craft` module's canonical systems" and `world.rs:1-6` "The `regolith.world` module's canonical systems … owns the `Rock`, `Pickup` and `BloomDirector` sections of `RegolithState`". Declared to the composition root at `regolith/mod.rs:453` `REGOLITH_MODULES` and validated at `:1909` `orrery_compose::validate(&REGOLITH_COMPOSITION)` |
| **S5** `SimulationHost` seam | **Done** | Crate `crates/orrery_sim_host` exists with `SimulationHost`, `SimulationHostConfig`, `RulesetAdapter`, `HostSnapshot` and a ruleset-generic C ABI (`src/abi.rs:1`). Already consumed outside its own tests: `crates/orrery_authority/src/hit.rs:568`, `:657`, `:665`. Recognised by the gate as a declared host: `scripts/core-gates.sh:528` `DECLARED_HOST_CRATES=(orrery_sim_host)` with the Tier-H harness list at `:540-547` |
| **S6.a** regolith `drive_core` | **OPEN** | `clients/regolith/src/lib.rs:1358` `fn drive_core` is still the hand-rolled loop, taking eleven `Res`/`ResMut` parameters and stepping the session directly. Neither `clients/regolith/Cargo.toml` nor the file names `orrery_sim_host` |
| **S6.b** campaign `advance` | **OPEN** | `clients/regolith/src/campaign.rs:1448` `pub fn advance` — same, no host seam anywhere in the crate |
| **S6.c** p1-swarm `step_core` | **OPEN** | `gates/p1-swarm/src/bot.rs:1152` `pub fn step_core(&mut self, tick: u64, cell_edge_m: f32)`; `gates/p1-swarm/Cargo.toml` does not depend on `orrery_sim_host` |
| **S7** ECS behind the seam | **Open by construction, and well ahead of A18's snapshot** | A18 §5's S7 note ends at #855; main has gone further. `bevy_ecs` is a first-class dependency of `orrery_games` (`crates/orrery_games/Cargo.toml:40`) and the ECS surface is now three files, not one: `regolith/world_ecs.rs`, `regolith/craft_ecs.rs`, `regolith/native_ecs.rs`. The two facts A18 records as "still true" **remain true**: `scripts/core-gates.sh:266` `BEVY_PERMITTED_CRATES=(orrery_games)` and `:528` `DECLARED_HOST_CRATES=(orrery_sim_host)`. Every byte-producing stage is still in core (`crates/orrery_core/src/executor.rs:480` `canonical_step_with`). D42 (d) leaves the exit unspecifiable and that has not changed |

## 3. The "decision recorded but condition unbuilt" shape

This is the category that is invisible to anyone reading either the plan or the
ADRs alone, and it runs in **both** directions. The tracker cannot see either
one: in both cases an issue closed and a merge happened.

### 3.1 Decision ahead of code — the live one

**S0.b.** The owner ruled on 2026-08-31 (#628) that overflow-canonicity is a
*per-ruleset declaration*, not a blanket requirement, and the ruling's whole
force is in one sentence: the declaration is "a `Ruleset` trait obligation with
**no default** … so a ruleset that does not state `true` or `false` fails to
compile" (`docs/adr/0043-…:556-562`). The documentation shipped. The obligation
did not. On main today a new ruleset can be written, compiled, registered and
shipped without ever stating whether its arithmetic overflow is canonical
state — which is precisely the "unrepresentable rather than merely
undocumented" property the amendment was chosen for. Skirmish correctly has no
`arithmetic_overflowed` field (zero hits under `crates/orrery_games/src/skirmish/`),
so the scoping half is real; it is the *enforcement* of scoping that is absent.

- **What is missing, concretely:** an undefaulted associated item on
  `Ruleset` (`crates/orrery_core/src/ruleset.rs:294`), plus a declaration at
  every impl site. There are **~30 `Ruleset` impls** across `orrery_core`
  (`src/executor.rs`, `src/sched.rs`, `tests/adjudication.rs`,
  `tests/round_trip.rs`, `tests/delta_codec.rs`), `orrery_games`
  (`regolith/mod.rs:689`, `skirmish/mod.rs`, `tests/materialization.rs`),
  `orrery_conformance` (`src/ruleset.rs:255`, `tests/quantize_pin.rs:89`),
  `orrery_authority` (`src/hit.rs:620`) and `orrery` (`src/lib.rs:797`).
  Mechanical, but wide, and it is a compile break by design — which is the
  point, and also the reason it did not ride along with the doc.
- **Status:** in flight as PR **#1099** (`feat/626-s0-d43-unbuilt-halves`),
  OPEN and unmerged at audit time. **No second lane should be dispatched at
  this.**

### 3.2 Code ahead of decision — three records that now lie by omission

The same failure mode, mirrored: the mechanism landed and the record that says
it does not exist was never updated. Each of these is a sentence someone can
cite next week to conclude that work is still owed.

| Record | What it still says | What is true on main |
|---|---|---|
| `docs/adr/0045-…:242`, `:319`, `:380-381` | IV-7 has "no mechanism exists today"; "**Until at least one lands, IV-7 is review-held**"; and MV-3 at `:402` is recorded as **Survived** with "no named check exists" | The compile-time bound landed. `crates/orrery_replicon/src/lib.rs:47` plus the guarded `replicate` / `replicate_diff` seam at `:426-438`, plus a committed trybuild `.stderr` that names `Entity: EngineHandleFree` as unsatisfied. MV-3 would now die at compile time at the registration call site — exactly the acceptance A9 §3 demanded and A18's S3.c detector restated |
| `docs/adr/0049-…:454`, and `:54`'s section heading "The digest everything routes on is a placeholder constant" | the digest "remains the placeholder Context §1 describes" | Both placeholders are gone. `RULESET_DIGEST` is build-script-computed over a verified manifest closure (`crates/orrery_ruleset_digest/src/lib.rs:154-161`) and consumed by both rulesets. D49's own warning about a hand-incremented constant being "one habit away from a plausible-looking lie" has been discharged |
| `docs/adr/0043-…:742-744` | Open questions item 1: "**(f)(4): wrapping or saturating** … Undecided; implementation of clause (f) blocks on it" | This is the *original* A18 finding and it is **still unrepaired**: `crates/orrery_games/src/regolith/mod.rs:1573` `flagged_add` saturates, and the tree is full of `saturating_*`. Implementation did not block — it chose, and has since shipped a computed digest over the choice. The repair is still a line in D43 recorded by the owner, not an inference from a diff |

None of these three is a code defect and none should be "fixed" by touching
code. They are docs-only, window-safe, and each is roughly a paragraph.

### 3.3 A structural gap the tracker cannot show

**S6 has no issues.** #626 said "S6 (driver convergence, three lanes) and S7
(conditional) get children when #642 exits", on the reasoning that filing them
early would file blocked issues against files taking twenty commits a week.
#642 (S5) has exited — `orrery_sim_host` is on main and already consumed by
`orrery_authority`. The children were never filed. The consequence is that the
epic reads **16/16 complete** while a whole stage of its own table is
untracked, unstarted, and now unblocked. That is why an auditor reading the
issue list concludes A18 is finished.

## 4. Stale citations in A18

A18's header claims every `path:line` was opened before being cited, and on
2026-08-28 that was true. Eight days later most of them are wrong. This matters
more than usual here, because A18's §2 corrects A11 for exactly this failure —
and A18's correction is now itself stale.

**Superseded — the cited code no longer exists in that form, because the
finding is closed:**

| A18 cites | Was | Is now |
|---|---|---|
| `scripts/core-gates.sh:37` `GATED_CRATES=(…)` | the typed array S2 replaces | `set -euo pipefail`. The floor is `:54`, discovery `:175` |
| `crates/orrery_persist_client/src/feed.rs:80-89` `tick: Tick::new(*seq_num)` | OD-21's live bug | `:90` `tick: universe_tick.tick` — S3.a landed |
| `crates/orrery_games/src/regolith/mod.rs:253-256` `digest: [0x66; 32]` | the drifting placeholder | `:254` `CAMPAIGN_ROCK_TIERS`. The digest is `:434-440`, computed |
| `crates/orrery_games/src/regolith/mod.rs:328-331` and `skirmish/mod.rs:106-115` inline `ComponentTypeId`s | "no registry and no duplicate refusal" | `:328` is engagement-range prose; `skirmish:106` is a version rationale. The registry is `orrery_compose::registry`, referenced at `regolith/mod.rs:635` |
| `crates/orrery_conformance/src/corpus.rs:60-96` "the same five cases" | five | seven — `:103` and `:114` are S1.c's |
| `crates/orrery_persistd/src/migration.rs:479-510` "synthesizes bags in-process from `b\"old\"`" | the insufficient self-check | `:474-478` include committed hex fixtures; `:483` documents their provenance |
| `crates/orrery_games/src/golden.rs` "commits state-hash chains only" | the blind spot | outcome tables at `:87`, `:180`, `:243` |
| `crates/orrery_witness/tests/detection.rs:1029`, `:1191` "checks monotone growth, not exact advance" | the gap | `:997` is the three-leg pin S1.b added |
| root `Cargo.toml` "**no `[profile]` section**" | the (f)-i residual | `:208-212` |

**Simply moved — same content, different line (cite the right one):**

| A18 cites | Correct line at `23c6e41` |
|---|---|
| `scripts/p4-ledger.sh:790-795` `PIPELINE_TREES` | **`:1218-1223`**. *A18's §2 corrected A11's `:409-414` for this exact reason; the correction has itself gone stale in eight days. The contents are still the four trees* |
| `scripts/p4-ledger.sh:797-811` `pipeline_id` | **`:1225-1239`** |
| `crates/orrery_core/src/ruleset.rs:233-333` the `Ruleset` trait | **`:294-393`** |
| `crates/orrery_core/src/ruleset.rs:255` `max_neighbor_reads` / `:263` `max_neighbor_staleness_ticks` | **`:316`** / **`:324`** |
| `crates/orrery_protocol/src/verifiable.rs:116-130` `RecordSource::NeighborFrame` | **`:186`** |
| `crates/orrery_games/src/regolith/state.rs:94-95` and `:220-221` `pub arithmetic_overflowed` | **`:270`** and **`:396`** |
| `crates/orrery_games/src/regolith/mod.rs:1661-1673` `flagged_add` | **`:1573`** |
| `docs/adr/0043-…:354-410` clause (f), `:376-380` the divergence argument | clause (f) heads at **`:528`**; the argument is quoted at **`:566`** and stated at **`:606`**. *`:355` is now a different 2026-08-31 amendment, against D42 clause (d)* |
| `crates/orrery_games/src/golden.rs:45` `REGOLITH`, `:83` `REGOLITH_PICKUP_CONTEST`, `:102` `SKIRMISH` | **`:51`**, **`:125`**, **`:144`** |
| `clients/regolith/src/lib.rs:982` `drive_core` | **`:1358`** |
| `clients/regolith/src/campaign.rs:935` `advance` | **`:1448`** |
| `gates/p1-swarm/src/bot.rs:741` `step_core` | **`:1152`** |
| `crates/orrery_persist_client/src/feed.rs:193` `local_granted_without_marker_is_not_uplinked` | **`:276`** |
| `crates/orrery_games/tests/battery.rs:26-37` `game_test!` | **`:34`** |
| `scripts/check.sh:90-103` `WORKSPACES` | **`:92`**; the self-test A18 cites at `:658` is at **`:796`** |
| `scripts/core-gates.sh:162-200` audited-predicate corridor, one entry `regolith/visibility.rs::verify_claims` | the entry is at **`:458`**. *Contents unchanged — still exactly one* |
| `scripts/core-gates.sh:259` `BEVY_PERMITTED_CRATES` (S7 paragraph) | **`:266`** |
| `crates/orrery_conformance/tests/quantize_pin.rs:130` / `:191` | **`:131`** / **`:192`** — off by one; both cite the `#[test]` attribute, and both named functions are correct |
| `crates/orrery_protocol/src/gateway.rs:379` | **`:378`** — off by one |
| `docs/10-crates.md:3` "fifteen at present" | `:3` now reads **"twenty at present"**; `:132`, `:72` and `:32` point at unrelated lines. The whole DA-1 row needs re-derivation before it is cited again |

**Still exactly right** — worth naming, because the drift is not uniform:
`crates/orrery_protocol/src/persist.rs:38-47`; `docs/adr/0021-…:20-21`;
`crates/orrery_witness/src/witness.rs:896` `shown_ticks += advance`;
`crates/orrery_core/src/log.rs:78-82` and `:605`;
`crates/orrery_core/src/executor.rs:56-58`; `:480 canonical_step_with`;
`Cargo.toml [workspace]:1`.

**One stale citation outside A18, found on the way and worth repairing at the
source:** D21's own dated correction (`docs/adr/0021-…:28`) cites the trait as
`crates/orrery_core/src/ruleset.rs:267-368`. It is at `:294-393`. A correction
whose citation rots is the shape it was written to prevent.

**A stale citation in the dispatch brief that produced this audit:** it lists
`PIPELINE_TREES` as `crates/orrery_games`, `gates/p1-swarm`, `crates/orrery_core`
and `crates/orrery_conformance`. At `scripts/p4-ledger.sh:1218-1223` the fourth
tree is **`crates/orrery_witness`**, not `orrery_conformance`. That changes a
window-safety answer: `orrery_conformance` is window-*safe*.

## 5. Window safety of what is open

`PIPELINE_TREES` at `scripts/p4-ledger.sh:1218-1223`, read at source:
`crates/orrery_witness`, `crates/orrery_core`, `crates/orrery_games`,
`gates/p1-swarm`.

| Open item | Touches | Window-safe? |
|---|---|---|
| **S0.b remainder** (PR #1099) | `crates/orrery_core/src/ruleset.rs` plus ~30 impl sites, including `crates/orrery_games` | **No** — two of the four trees directly, and the break propagates workspace-wide |
| **S6.a** `clients/regolith/src/lib.rs` | `clients/` | **Yes** |
| **S6.b** `clients/regolith/src/campaign.rs` | `clients/` | **Yes** |
| **S6.c** `gates/p1-swarm/src/bot.rs` | `gates/p1-swarm` | **No** |
| **§3.2 ADR repairs** (D43 (f)(4), D45 IV-7, D49 OD-22) | `docs/adr/` only | **Yes** |
| **S7 continuation** | `crates/orrery_games` | **No** |

So if a freeze window is declared, **S6.a, S6.b and the three ADR repairs
remain workable**, and everything else stops. S0.b's remainder is the awkward
one: it is the only genuinely open *code* obligation, it is small in intent,
and it is unschedulable inside a window by construction. That is an argument
for landing #1099 before a quiet point is declared — not for expanding it.

## 6. What a reader should do with this

- **Do not dispatch a lane at S0.a, S0.c, S1.a–e, S2, S3.a–d, S4.1, S4.2 or
  S5.** All are on main with named checks. §2 gives the line to check first.
- **S0.b:** #1099 is the lane. It is open. Do not open a second.
- **S6:** file the three children #626 deferred, then run S6.a and S6.b; hold
  S6.c on `gates/p1-swarm` quiescence, which was its stated entry condition and
  is still the right one.
- **§3.2:** three ADR paragraphs, docs-only. Cheap, and each currently reads as
  a standing obligation that is already discharged.
- **A18 itself stays as written.** It is a dated record of 2026-08-28 and this
  node is the diff against it; §4 is the list to consult before quoting any
  line of it.

## Cross-references

- Plan audited: [A18](a18-ruleset-ecs-implementation-programme.md) (#625)
- Epic: [#626](https://github.com/baadc0de/orrery/issues/626)
- In flight: [#1099](https://github.com/baadc0de/orrery/pull/1099)
- Records touched by §3.2:
  [D43](../adr/0043-determinism-envelope-and-gate-replacement.md),
  [D45](../adr/0045-per-component-capability-policy.md),
  [D49](../adr/0049-compatibility-manifest.md)
