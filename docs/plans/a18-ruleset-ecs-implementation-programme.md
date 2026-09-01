# A18 - The Ruleset/ECS implementation programme: eight stages, re-sequenced off the digest

> Grooming for [#395](https://github.com/baadc0de/orrery/issues/395), which
> closed its drafting phase on 2026-08-25 with eight Accepted records
> (D42-D49) and one instruction: "Implementation receives a separate epic,
> and only after the owner accepts the relevant ADRs." The records are
> accepted; this node is the separate epic's plan. Repository facts verified
> in this worktree at `f82ee980` on 2026-08-28; every `path:line` below was
> opened before being cited, because this corpus drifts hard - A11's own
> `p4-ledger.sh:409-414` citation is three days old and now points at
> different code. Nothing here amends an accepted record - **propose, not
> decide.** Section 11 lists what stays with the owner; section 10 lists what
> could not be verified.
>
> Series placement: A-series because it consumes A1-A11 and produces the
> tranche successor, not a shippable design. It supersedes nothing: A11
> section 5's tranche table remains the record of what was planned on
> 2026-08-25, and section 2 below is the diff against what is true today.
>
> **The epic this node plans is
> [#626](https://github.com/baadc0de/orrery/issues/626)**, filed with sixteen
> children on 2026-08-28. Where a stage below names lanes, the issue numbers
> are in that epic's child table.

## 1. Verdict up front

**A11's decomposition is sound and its sequencing is not.** The PR contents,
the acceptance criteria, and the F-2-before-Phase-2 rule survive re-derivation
without a change. The *ordering* was built on one axis - membership of
`PIPELINE_TREES`, the P4 pipeline digest - and that axis is inoperative today
and points the wrong way tomorrow. Three verified facts:

1. **Nothing is banked.** `p4-ledger.sh total` has reported nothing banked at
   every check since 2026-08-25 (#329 comments of 2026-08-25T12:53 and
   2026-08-25T19:12), and the ledger file `P4_LEDGER_FILE` defaults to
   `target/p4-ledger/hours.jsonl` (`scripts/p4-ledger.sh:65`) - build output,
   untracked, absent. A digest reset costs the number of hours it resets, and
   that number is zero.
2. **The window has never been declared.** #329 is OPEN and titled "then open
   banking inside a declared freeze window"; its most recent substantive
   comment (2026-08-27T13:02) is a *proposal* whose first line is "Declaring
   the freeze window and opening banking are the owner's acts; nothing here is
   decided", with two of six exit criteria "not started".
3. **The trees are being touched anyway, hard.** Since 2026-08-25 there are
   25 commits into `PIPELINE_TREES`: 10 into `crates/orrery_games`, 20 into
   `gates/p1-swarm`, 3 into `crates/orrery_core`. The constraint A11 planned
   around is one nobody is currently observing, because there is nothing to
   observe it for.

So "post-window" as a scheduling primitive is empty. What is *not* empty is
the future: once the owner declares the window, the same four trees become
untouchable for its duration, and #329's own blocker B3 asks for the tree to
be quiet *before* it opens. That inverts A11's rule exactly. **The two
digest-tree fixtures A11 deferred to "the first post-window batch" (PR-9,
PR-10) are the two items that most need to land before the quiet point**, and
one of them - PR-9, the outcome-chain goldens - gates every behaviour-changing
stage in the programme.

**Recommended sequencing, in one line:** land F-2 now (S1); run everything
window-safe in parallel while the owner decides about the window (S2, S3);
hold the composition root and the host seam until the shakedown is done or
explicitly deferred (S4-S6); leave the ECS itself trigger-gated where D42
clause (d) put it (S7).

**What I decline:** A11's PR-16 as a unit (driver convergence) - it is
correctly flagged in A11 section 11.3 as the least-specified item, and
`gates/p1-swarm` has taken 20 commits in three days, so a convergence PR
written today would be re-written by merge conflict before review. It is split
three ways and gated on quiescence in S6. I also decline the "tranche 1 needs
no ADR" framing for the gate work: D43 clause (d) is Accepted, so S2 is not
ungated - it is *gated and cleared*, which is a different and better state.

## 2. The staleness audit: what moved under the plan in three days

A11 was written on `2b542c4d`. Every row below was re-verified at `f82ee980`.
"Landed" means the plan's acceptance criterion is met on main today.

| A11 item | Status at HEAD | Evidence |
|---|---|---|
| PR-0 DA-1 (docs/10 census) | **Landed** | `docs/10-crates.md:3` "fifteen at present"; `orrery_conformance` in the reference table at `:132`; `orrery_field_host` annotated "planned P6, not built" at `:72` and absent from the table; `aeronet_iroh` under `vendor/` at `:32`. #414 CLOSED |
| PR-0 DA-2 (D21 footnote) | **Not started** | `docs/adr/0021-ruleset-distribution.md:20-21` still reads "`Ruleset::validate_intent` behind `intent::IntentValidator`"; the method is not a member of the trait (`crates/orrery_core/src/ruleset.rs:233-333`) |
| PR-0 `persist.rs` tense | **Landed** | `crates/orrery_protocol/src/persist.rs:38-47` now says the block grant "is designed ... but not yet built (D44)" |
| PR-1 / F-3 quantize pin | **Landed** | `crates/orrery_conformance/tests/quantize_pin.rs:130` `the_claimed_hash_is_of_the_quantized_state_not_the_raw_one`, plus an anti-vacuity twin at `:191`. Pins `crates/orrery_core/src/executor.rs:173-174`. PR #426 merged |
| PR-2 corpus cases | **Not started** | `crates/orrery_conformance/src/corpus.rs:60-96` holds the same five cases; no `projection-order-permuted`, no `swarm-large` |
| PR-3 / F-7 #417 closure | **Partial (1 of 3)** | `crates/orrery_persist_client/src/feed.rs:193` `local_granted_without_marker_is_not_uplinked` landed, with OD-31's reachability call written into `:174-191`; the guard is de-shadowed into its own param at `:62-66`. The handoff and creation/destruction scenarios are absent |
| PR-4 / F-6 migration goldens | **Not started** | `crates/orrery_persistd/src/migration.rs:479-510` still synthesizes bags in-process from `b"old"` - the encode-decode-encode self-check A10 named as insufficient. No committed old-format bytes anywhere in `crates/` |
| PR-5 / F-12 bench workspace | **Not started** | No `gates/migration-bench`; `scripts/check.sh:90-103` `WORKSPACES` unchanged; `docs/plans/baselines/` does not exist |
| PR-6 Tier V discovery | **Not started (the half that matters)** | `scripts/core-gates.sh:37` is still `readonly GATED_CRATES=(orrery_core orrery_games orrery_conformance)`. The *neighbour* half was rewritten twice by owner amendment (see section 3.2) |
| PR-7 / OD-21 `DiffUplink.tick` | **Not started; bug live** | `crates/orrery_persist_client/src/feed.rs:80-89` still writes `tick: Tick::new(*seq_num)` and `seq: tick` from the same counter, while `crates/orrery_protocol/src/gateway.rs:379` still documents "The universe tick at append (D8)" |
| PR-8 / OD-26 / F-9 | **Not started** | Zero hits for `EngineHandleFree` or `trybuild` in first-party sources |
| PR-9 / F-2 outcome chains | **Not started** | `crates/orrery_games/src/golden.rs` commits state-hash chains only (`REGOLITH` `:45`, `REGOLITH_PICKUP_CONTEST` `:83`, `SKIRMISH` `:102`); the blind spot is documented in the module header at `:21-28` |
| PR-10 / F-8 witness pin | **Not started** | `crates/orrery_witness/src/witness.rs:896` `shown_ticks += advance` with the contract at `:116-127`; the only nearby assertion (`tests/detection.rs:1029`) checks monotone growth, not exact advance, and `:1191` covers a different mechanism |
| PR-11..PR-14 composition | **Not started** | No composition-root type, no manifest struct, no `ComponentTypeId` registry file (ids are inline: `regolith/mod.rs:328-331`, `skirmish/mod.rs:106-115`), no manifest keyspace family in `crates/orrery_persistd/src/keyspace.rs`, digests still placeholders |
| PR-15/PR-16 host seam | **Not started** | `SimulationHost` appears only in `docs/`; three hand-rolled drivers remain (`clients/regolith/src/lib.rs:982`, `clients/regolith/src/campaign.rs:935`, `gates/p1-swarm/src/bot.rs:741`) |

**Corrections to A11's own text, found while verifying:**

- **`PIPELINE_TREES` has moved.** A11 section 10 cites `scripts/p4-ledger.sh:409-414`
  and says so precisely. At HEAD the array is at **`:790-795`**; `:409-414`
  now lands in unrelated code. The *contents* are unchanged (the four trees),
  and `scripts/p4-attempt-accounting.py:873-877` cross-checks the block
  against its own copy, so the two cannot drift from each other - only from
  citations.
- **The Regolith placeholder digest has drifted.** A11 and D49 cite
  `[0x63; 32]`; `crates/orrery_games/src/regolith/mod.rs:253-256` now reads
  `RulesetId { version: 16, digest: [0x66; 32] }`. It moved with ordinary
  version bumps, not through X-1 computation. It is still a placeholder, and
  the claim it supports (nothing computes a digest) is unchanged - but a
  hand-incremented constant is one habit away from D49's "plausible-looking
  lie", and that is worth naming now rather than after it looks real.
- **The commissioning brief's description of the gate is stale.** #395's
  constraints section says `core-gates.sh` "bans `view.neighbor(` in the
  rules crates". It no longer does. Since #468 and #598 it *permits* reads
  inside declared audited predicates and refuses everything else
  (`scripts/core-gates.sh:162-200`, one entry today:
  `crates/orrery_games/src/regolith/visibility.rs::verify_claims`). The ban
  became a tripwire, by owner amendment recorded in D43 clause (d). Anyone
  planning against the categorical form is planning against 2026-08-24.

## 3. The two flagged gaps from #447, re-verified

#395's comment of 2026-08-25T13:16 recorded two findings from the collision
design. **Both are closed at HEAD.** Neither was closed by this programme -
both were closed by the campaign work that ran through them.

### 3.1 D43 clause (f)'s overflow flag - closed, three residuals

The clause (`docs/adr/0043-determinism-envelope-and-gate-replacement.md:354-410`)
requires occurrence to reach witnessed state: "if overflow set a bit that
hashing never sees, two hosts could diverge - one flagged, one not - while
`hash(e, t)` still matches" (`:376-380`).

The field exists. `crates/orrery_games/src/regolith/state.rs:94-95` and
`:220-221`:

```rust
/// Canonical arithmetic overflow has occurred in this entity's rules.
pub arithmetic_overflowed: bool,
```

It is inside the codec - pushed at `:364` and `:437`, decoded at `:410`
(craft, byte 131) and `:468` (rock, byte 83) - therefore inside
`bytes(e,t) = CoreCodec::encode(quantize(state(e,t)))` and therefore inside
`hash(e,t) = blake3(bytes(e,t))`, which is what WP-1 commits. It is set at
`crates/orrery_games/src/regolith/mod.rs:439-445` and `:1091-1110` through

```rust
fn flagged_add(left: i64, right: i64, overflowed: &mut bool) -> i64 {
    left.checked_add(right)
        .unwrap_or_else(|| { *overflowed = true; left.saturating_add(right) })
}
```

(`mod.rs:1661-1673`). It landed against #441, whose body carries the
requirement verbatim; #441 is CLOSED.

**Residual (f)-i: the profile pin does not exist.** Clause (f)(2) requires
that "the canonical crates' build pins `overflow-checks = false` uniformly
across profiles, so that any stray plain operation the review missed behaves
*identically* on every host and profile". There is **no `[profile]` section in
the workspace root `Cargo.toml`** - its sections are `[workspace]:1`,
`[workspace.dependencies]:56`, `[workspace.lints.clippy]:128`,
`[patch.crates-io]:150`, `[workspace.lints.rust]:153`. Cargo's defaults
therefore apply: `overflow-checks = true` in dev, `false` in release. That is
precisely the profile split Context section 3 of D43 demonstrated (dev panics,
release wraps to `-2147482649`) and clause (f)(2) exists to close. Half of the
accepted clause is unimplemented, and it is a four-line change.

**Residual (f)-ii: the flag is Regolith-only.** `arithmetic_overflowed` does
not appear anywhere under `crates/orrery_games/src/skirmish/`, which does
`i64` arithmetic and carries `saturating_` calls in `invariants.rs`,
`pilot.rs` and `mod.rs`. Clause (f)(3) says "a per-entity discrete field of
canonical state" without scoping it to one ruleset. Either Skirmish owes the
field, or the clause is per-ruleset and someone should say so. Both readings
are defensible; choosing is not mine (section 11).

**Residual (f)-iii: (f)(4) is open and the code has already answered it.**
D43's Open questions item 1 says wrapping-vs-saturating is "Undecided;
implementation of clause (f) blocks on it", and clause (f)(4) says "no
canonical arithmetic may be written that depends on which one wins".
`flagged_add` saturates. 103 `saturating_` calls exist under
`crates/orrery_games/src/`. The implementation is not blocked - it chose. This
is not an accusation that anything is wrong: saturating is very likely the
right answer for a game where a clamped velocity is meaningful and a wrapped
one is a teleport. It is a governance discrepancy: **the record says the
decision is owed and the tree has already spent it**, and the honest repair is
the owner recording the choice, not an agent inferring it from the diff.

### 3.2 `NeighborFrame` declared tick and staleness cap - closed

Both halves landed through #444/#457 and #441/#468, and D43 clause (d) was
amended twice by the owner to match.

- **Declared tick.** `crates/orrery_protocol/src/verifiable.rs:116-130`:
  `RecordSource::NeighborFrame { neighbor, present, observed_tick }`, with the
  hazard written into the doc at `:124-128`: "This is deliberately not the
  reader's tick: replication lag is ordinary, and cross-checking the payload
  against a claim from a newer tick would manufacture evidence against an
  honest peer." The in-memory twin is `crates/orrery_core/src/executor.rs:56-63`.
- **Staleness cap.** `crates/orrery_core/src/ruleset.rs:263`
  `fn max_neighbor_staleness_ticks(&self) -> u64 { 0 }` - fail-closed default,
  paired with `max_neighbor_reads` at `:255`. Enforced at
  `crates/orrery_core/src/replay.rs:230-257` and, before any state hash is
  compared, in `crates/orrery_core/src/log.rs:78-107`
  (`cross_check_neighbor_record`), whose contract at `:80-82` is "Every error
  means 'refuse this cross-check', never 'convict the reader'". Pinned by
  `log.rs:605` `neighbor_cross_check_uses_declared_tick_and_refuses_stale_frames`.
  Regolith's bounds were tightened to the reads that exist in `2b80cef0`
  (`crates/orrery_games/src/regolith/mod.rs:392-396`).

The false-accusation mechanism #447 warned about is closed at the layer that
can close it. **#447's sharper claim also stands and nothing below assumes
otherwise**: closing the frame gap upgrades *detection* only; exact
conservation of a two-party interaction still needs a carried impulse. No
stage in section 5 depends on recomputed impulses.

## 4. Why the digest axis inverts, with the arithmetic

The pipeline digest is a hash of four git trees and nothing else
(`scripts/p4-ledger.sh:797-811`):

```
pipeline_id(commit) = sha256( concat_over t in PIPELINE_TREES of
                                "t=" ++ git rev-parse commit:t ++ "\n"
                            )[0..16]
```

with `PIPELINE_TREES = (crates/orrery_witness, crates/orrery_core,
crates/orrery_games, gates/p1-swarm)` (`:790-795`). `total` groups banked
hours by this id, so a change to any of the four does not delete hours - it
*partitions* them, and the largest group is what counts.

Let `B(t)` be hours banked at time `t`, `W` the declared window, and `C` the
set of commits into `PIPELINE_TREES`. The cost of one such commit is:

```
cost(c) = B(t_c)                       if banking is open
        = 0                            otherwise
```

Today `B(t) = 0` for all `t`, so `cost(c) = 0` for every commit in section 2's
list of 25. **A11's tranche 1/tranche 2 split priced a cost that is zero and
will stay zero until the owner opens banking.** That is not a criticism of
A11 - on 2026-08-25 the window looked imminent - it is a fact about the plan's
load-bearing axis three days later.

The real cost function is the *other* one. Let the window open at `t_0` with
duration `W`, and let item `i` have digest-tree scope `d_i in {0,1}` and
duration `w_i`. Then:

```
delay(i) = 0                if d_i = 0                     (window-safe)
         = 0                if d_i = 1 and finished < t_0  (beat the quiet point)
         = W + (t_0 - now)  if d_i = 1 and started > t_0    (waits out the freeze)
```

#329's proposed sequence asks for a declared quiet point with the commit hash
recorded (step 3 of the 2026-08-27 comment) and requires open lanes touching
frozen paths to "land or be abandoned before the window opens" (blocker B3).
So a digest-tree item is cheap now, free-but-late if it waits, and *actively
obstructive* if it is in flight when the owner wants to declare. There is no
window in which starting one is a good idea.

Two items in this programme have `d_i = 1` and matter:

- **F-2 outcome chains** (`crates/orrery_games`), `w = 1-2 days`. It gates
  every behaviour-changing stage by A10's own rule, and it has a second clock
  on it - see below.
- **F-8 witness re-delivery pin** (`crates/orrery_witness`), `w = 1 day`. It
  gates nothing; `crates/orrery_witness` has taken zero commits since
  2026-08-25, so it is the cheapest tree to enter and the cheapest to defer.

**F-2's second clock is the argument for urgency, and it is specific to F-2.**
Outcome goldens must commit *legacy* behaviour - that is the whole point of
A10's "F-2 lands before any Phase 2 composition PR" rule (A10 section 8.3).
Legacy behaviour is not standing still: `crates/orrery_games` has taken 10
commits in three days, `REGOLITH_RULESET.version` reached 16, and goldens were
regenerated at least twice (`5f7e2194`, breaking; `a53765fa`). Write the
chains at `t`, and what they pin is `legacy(t)`. Delay by `d` and they pin
`legacy(t+d)`, a different function, against which the composition root's
parity argument is a different argument. Delay does not make F-2 later; it
makes F-2 *about something else*. Nothing in the rest of the programme has
that property.

## 5. The stages

Eight stages. Each has an **entry condition** (what must be true to start),
an **exit condition** (what must be true to stop), and a **detector** - the
named, mechanical thing that says the stage actually worked, as opposed to
compiled. Sizes are lanes: one person, one to two days, one merge.

Stage identifiers are S0-S7. They are not the canonical stage set S0-S7 of
D43 clause (b) - that collision is unfortunate and this document uses
"stage Sn" for programme stages and "canonical stage" for D43's throughout.

### S0 - Discharge the accepted records' unimplemented halves

Not migration work. Two clauses of D43 that were accepted and only half built,
plus one doc footnote A11 planned and nobody wrote. All small, all disjoint,
none blocked on anything.

| Lane | Scope | Files (exclusive) |
|---|---|---|
| S0.a | `[profile.*] overflow-checks = false` across dev/release/test/bench, plus one test proving canonical arithmetic does not panic under a debug build | root `Cargo.toml` (new `[profile]` sections only); `crates/orrery_conformance/tests/overflow_profile.rs` (new) |
| S0.b | Skirmish overflow-flag parity, **or** a recorded per-ruleset scoping in the PR body if the owner rules the clause Regolith-scoped | `crates/orrery_games/src/skirmish/**` only - **not** `tests/battery.rs` |
| S0.c | DA-2: dated footnote on D21's `validate_intent` parenthetical | `docs/adr/0021-ruleset-distribution.md` |

- **Entry:** none. D43 is Accepted; these are its unbuilt halves.
- **Exit:** every sub-clause of D43 clause (f) is implemented or named in
  section 11 with its blocker. (f)(4) will still be named - S0 does not close
  it, and S0.b may be gated on it.
- **Detector:** replace one `flagged_add` call in
  `crates/orrery_games/src/regolith/mod.rs` with a plain `+`. Under the pin,
  the behaviour is identical in dev and release and the profile-parity test
  passes for the wrong reason; **without** the pin the same mutation makes the
  dev build panic where release wraps. The test is written so the *absence* of
  the pin is what kills it: it asserts the release-profile result under a dev
  build. If it can pass with the `[profile]` section deleted, it is theater.

### S1 - Commit legacy behaviour before it moves

The parity instruments. This is A11's tranche 1 plus tranche 2, with the
digest ordering removed and one item promoted to first.

| Lane | Scope | Files (exclusive) | Digest tree? |
|---|---|---|---|
| **S1.a** | **F-2 outcome-chain goldens**: in-loop fold (A10 N-1) of events, materialized ids and delivery pairs, WP-2-ordered by ascending `PersistId`; committed `REGOLITH_OUTCOMES` / `SKIRMISH_OUTCOMES` | `crates/orrery_games/src/golden.rs`, `crates/orrery_games/src/scenario.rs`, `crates/orrery_games/tests/battery.rs` | **yes** |
| S1.b | F-8 witness re-delivery pin, three legs: deliver, re-deliver unchanged, extend advances exactly | `crates/orrery_witness/tests/**` | **yes** |
| S1.c | F-5 corpus legs: `projection-order-permuted` and `swarm-large` (256 entities) | `crates/orrery_conformance/src/corpus.rs`, `crates/orrery_conformance/corpus/golden.json` | no |
| S1.d | F-6 migration round-trips against **committed** old-format bytes; per-slot future-version refusal; module-removal refusal | `crates/orrery_persistd/src/migration.rs`, `crates/orrery_persistd/goldens/**` (new) | no |
| S1.e | F-12: `gates/migration-bench` workspace, role `check`, plus first baseline JSON with environment manifest | `gates/migration-bench/**` (new), `scripts/check.sh` (one `WORKSPACES` row), `docs/plans/baselines/**` (new) | no |

- **Entry (S1.a):** none, and it should start first. **Entry (S1.b-e):** none;
  all four are independent lanes.
- **Exit:** dual-chain pinning is live - state chains (exist) and outcome
  chains (S1.a) bracket every later move; the differential harness has a
  committed baseline to refuse against; every fixture has a demonstrated kill.
- **Detector, S1.a (this is the one that matters):** flip a `deliver` arm to
  `None` in Regolith's `step`. `outcome_chains_match_the_committed_golden`
  must fail on the emission tick **while `chains_match_the_committed_golden`
  stays green**. Both halves are the result: a kill proves sensitivity, and
  the state chain staying green proves the fixture is covering the gap
  `golden.rs:21-28` documents rather than duplicating F-1.
- **Detector, S1.b:** re-apply A6's M-A6-4a - remove the re-delivery immunity
  from the coverage fold at `crates/orrery_witness/src/witness.rs:896` - and
  leg 2 must die by name. Its inverse (advance forced to 0) must kill leg 3.
- **Detector, S1.e:** delete the committed baseline manifest; the harness must
  **refuse to run**, not run and compare against nothing.

### S2 - Replace the gate before anything needs it to be stronger

D43 clause (d), Tier V role-discovered membership. Window-safe (`scripts/` is
not a digest tree), owner-cleared (the record is Accepted), and the standing
bar - "a weaker gate that passes is worse than the current one" - is the
acceptance criterion, not a slogan.

- **Scope:** replace `scripts/core-gates.sh:37`'s typed `GATED_CRATES` with
  role discovery: scan for `impl .* Ruleset for` and `: Ruleset` bounds with
  `cfg(test)` stripped and qualified paths handled, then take the union of
  discovered and declared. The declared list stays as a floor; discovery adds.
  No Tier H clauses in this stage's scope. (At plan time D43 clause (e) armed
  only on a D42 trigger and none had fired; since then Tier H landed - #771 -
  and arms per declared host, `DECLARED_HOST_CRATES`, with the host admitted
  by owner sanction rather than a fired trigger.)
- **Files (exclusive):** `scripts/core-gates.sh`.
- **Entry:** none.
- **Exit:** on this tree discovery reproduces exactly
  `{orrery_core, orrery_games, orrery_conformance}` (A4's E-D1 result), and
  the full existing clause battery still runs over the same source set.
- **Detector, two-sided, both wired as CI self-tests:**
  1. Add a synthetic crate that implements `Ruleset` and is *not* in the
     declared list, with `bevy_ecs` in its graph. The gate must fail naming
     that crate. (Under today's script it passes - that is the hole.)
  2. Remove a crate from the declared list while it still implements
     `Ruleset`. The gate must still cover it. A gate whose coverage can be
     reduced by editing an array is the gate D43 clause (d) replaces.

### S3 - Dispose of the open findings the plan must not absorb

Four findings that A11 listed rather than absorbed, each independently
closable, none gating a later stage. They are grouped as a stage because they
share a property: each is a place where a documented claim and the code
disagree, and every week they survive is a week someone can cite them.

| Lane | Scope | Files (exclusive) | Blocked on |
|---|---|---|---|
| S3.a | OD-21: `DiffUplink.tick`. Either stamp the real simulation tick, or rename the semantic in the docs. Not both, not neither | code half: `crates/orrery_persist_client/src/feed.rs`; docs half: `crates/orrery_protocol/src/gateway.rs` | **OD-21, owner** |
| S3.b | PR-3's remaining two F-7 scenarios: handoff-adjacent-to-rollback, creation/destruction-in-window | `crates/orrery_predict/tests/**` | none |
| S3.c | OD-26 / IV-7: the `EngineHandleFree` bound or registry-time refusal, plus F-9's trybuild suite with committed `.stderr` and a positive twin | new `crates/*/tests/ui/**` at the registration seam | **OD-26, owner**; and a spike first - A9 section 9 could not confirm replicon's registration API admits the bound without forking |
| S3.d | X-1 digest computation, replacing both placeholders | `crates/orrery_games/src/{regolith,skirmish}/mod.rs` (digest constants only) | **OD-22, owner** |

- **Entry:** the named owner decision, per lane. S3.b has none and can run any
  time.
- **Exit:** each finding is closed or has a dated issue saying which owner
  decision it waits on.
- **Detector, S3.c:** re-apply A9's M3 - append `entity.to_bits()` to a
  replicated payload. It must fail **at the registration call site, at compile
  time**. Byte scanning is explicitly not an acceptable substitute: A9 section
  3's reasoning is that entity bits are indistinguishable from any other
  `u64`, so a scanner that passes is a scanner that proves nothing.
- **Detector, S3.d:** build twice with a one-character change to a
  determinism-relevant source file. The two `RulesetId.digest` values must
  differ. Build twice with no change: identical. A digest that does not move
  is the placeholder with extra steps.

### S4 - The composition root, behind the unchanged `Ruleset`

D42 clause (b)(1) and D49's manifest shape. The first stage that changes
structure, and the first that must not change behaviour.

- **Scope:** a new crate holding the plain struct-of-tables manifest (A8
  section 3.1), composition-time validation, and the reviewed per-game
  `ComponentTypeId` registry file that X-5 asks for - today the ids are inline
  constants (`crates/orrery_games/src/regolith/mod.rs:328-331`,
  `crates/orrery_games/src/skirmish/mod.rs:106-115`) with no registry and no
  duplicate refusal. Then, in a *second* lane, the first two Regolith domains
  split into delegated modules behind the one assembled `Ruleset`.
- **Files:** lane 1 is a new crate plus one registry file, touching no
  existing source. Lane 2 touches
  `crates/orrery_games/src/regolith/mod.rs` and must run alone.
- **Entry:** **S1.a merged** (non-negotiable - A10 section 8.3), and the
  campaign programme quiet enough in `crates/orrery_games/src/regolith/` that
  lane 2 is not rebased daily. See the argument against in section 9.
- **Exit:** D42 clause (b)(1)'s own criterion, unchanged from the brief's
  phase 2: at least two existing behaviours owned by separate modules; state
  **and** outcome chains byte-identical; `./scripts/core-gates.sh` exits 0.
- **Detector, lane 1:** delete the duplicate-schema-id refusal from
  composition validation. `duplicate_schema_id_refuses_composition` must die.
  Same for the cycle check against `cyclic_dependency_refuses_composition`,
  and the missing-dependency check against
  `missing_dependency_refuses_composition` - F-10's battery, and each of the
  three kills is demonstrated separately in the PR body.
- **Detector, lane 2:** this is the parity claim, so the detector is
  arithmetic rather than a mutation. For every scenario in the catalogue and
  every tick `t`:

  ```
  state_chain_after[t]   == state_chain_before[t]
  outcome_chain_after[t] == outcome_chain_before[t]
  ```

  byte for byte, with `before` read from the committed tables rather than
  regenerated. A regeneration in this PR is the failure, not the fix.

### S5 - The `SimulationHost` seam

D42 clause (b)(2). One kernel-owned driver: tick advance, stable-id lookup
(D44's single index), command-in/event-out, output collection.

- **Scope:** a new host crate; the existing `Ruleset` hosted through an
  adapter; headless tests driving the same API a client will. The host's
  storage stays clause (a)'s executor - **the seam moves no state**.
- **Files:** new crate only. No existing driver is converged here.
- **Entry:** S4 exit.
- **Exit:** the brief's phase-3 criteria, adopted verbatim by D42: a Bevy
  client test-double and the headless tests invoke one API; host lifetime and
  fixed-step semantics are explicit; goldens unchanged.
- **Detector:** make the host skip canonical stage S4 (quantization) on the
  path into hashing. `chains_match_the_committed_golden` must die, and
  `the_claimed_hash_is_of_the_quantized_state_not_the_raw_one` (S1's
  predecessor, `crates/orrery_conformance/tests/quantize_pin.rs:130`) must die
  with it. A seam that can drop a canonical stage without a named check
  noticing has re-opened X-C one layer up.

### S6 - Driver convergence, one driver per lane

A11's PR-16, split. Three hand-rolled loops exist:
`clients/regolith/src/lib.rs:982` (`drive_core`),
`clients/regolith/src/campaign.rs:935` (`advance`), and
`gates/p1-swarm/src/bot.rs:741` (`step_core`, with a second honest shadow
executor and the harness-side frame/claim assembly A11 wanted promoted).

| Lane | Driver | Files (exclusive) |
|---|---|---|
| S6.a | regolith `drive_core` | `clients/regolith/src/lib.rs` |
| S6.b | regolith campaign `advance` | `clients/regolith/src/campaign.rs` |
| S6.c | p1-swarm bots, including the frame/claim assembly | `gates/p1-swarm/src/bot.rs` |

- **Entry:** S5 exit, **and** the lane's own file quiet. S6.c additionally
  waits on the campaign programme: `gates/p1-swarm` took 20 commits in three
  days, and a convergence PR into that is a merge-conflict generator, not a
  refactor.
- **Exit, per lane:** the diff shows a hand-rolled loop **deleted**, not
  edited; p1-swarm gate criteria unchanged; goldens unchanged.
- **Detector:** each lane is revertable alone, and the revert is the detector.
  Revert the lane; the gate criteria and goldens must return to their
  committed values. A revert that does not is itself a finding (A11 section
  5.4, and I would keep that rule).

### S7 - Entry sanctioned 2026-08-30; the seam substrate landed 2026-08-31

D42 clause (d)'s automatic entry path is the pre-registered triggers
T1 (per-component storage measurably needed), T2 (measured tick cost
dominated by the `BTreeMap` store), T3 (Tier H landed and demonstrated at
least as strong as what it replaces), with **T3 a necessary precondition
regardless of T1/T2.** No trigger fired; the owner sanctioned S7 entry
directly on 2026-08-30 (#745). Tier H is no longer empty: it landed (#771),
its battery enforced mutation-style (`scripts/core-gates.sh` section 6), and
the first lane in landed the next day as `orrery_sim_host`'s `EcsBackend`
(#757), at four-class F-4 parity - the admission D42 clause (d)'s amendment
blockquote records. Where S7 stands now: the seam substrate is in; the
`orrery_games` side of the acceptance (#793, 2026-08-31 - `bevy_ecs`
first-class, ECS as idiomatic storage, systems and tick driving over
`World`) is recorded but the dependency is not yet taken - the manifest
declares no Bevy and `BEVY_PERMITTED_CRATES` carries the crate
(`scripts/core-gates.sh:259`).

- **Entry:** the owner's sanction (#745, 2026-08-30), not a fired trigger.
  Of the precondition package: Tier H demonstrated (#771); the four-class
  differential harness live (`crates/orrery_games/src/diff.rs`, A10 §4.1's
  four classes, generalized across substrates at #757); A5's capability
  registry partial (declaration data plane live; IV-7's engine-handle
  refusal and the persistd linkage open, D45); capacity-scale mirror
  numbers not met and partly overtaken (source half measured, docs/14
  §12.6; the consumer half does not exist).
- **Exit:** not specifiable before entry, and specifying it now would be the
  kind of unused specification D43 clause (e) already admits to carrying.
  The seam-substrate lane's acceptance, stated at its landing: four-class
  F-4 parity (#757).
- **Detector:** the trigger list remains the automatic path's detector, and
  it is a measurement, not a judgement. T2 in particular is arithmetic:
  publish the measured share of tick cost attributable to the store. A3
  measured the mirror cost the shared world would save at ~9 us per 10k
  entities, ~1.5% of a tick. A store that "feels slow" is not T2.

## 6. The dependency graph, and where it is actually serial

```
S0.a S0.b S0.c   S1.b S1.c S1.d S1.e   S2   S3.a S3.b S3.c S3.d
  |    |    |      |    |    |    |     |     |    |    |    |
  +----+----+------+----+----+----+-----+-----+----+----+----+   all parallel
                                  |
                   S1.a  ---------+  (parallel to all of the above;
                     |               first to start, gates S4)
                     v
                    S4 lane 1  ->  S4 lane 2
                                        |
                                        v
                                       S5
                                        |
                          +-------------+-------------+
                          v             v             v
                        S6.a          S6.b          S6.c
                                        |
                                        v
                                       S7 (entry sanctioned 2026-08-30; #757 landed)
```

Twelve of the sixteen non-conditional lanes are parallel. **The serial spine
is exactly four deep**: S1.a -> S4 lane 1 -> S4 lane 2 -> S5. Everything else
is width. This is the shape A11 produced and it is right; the change here is
which end of it starts.

## 7. Mutation-check discipline

Every child issue carries a MUTATION CHECK naming **the guarded stage to
break**, not the check line to edit. The distinction is the whole method and
this tree has been burned by the other version:

- Breaking the *check* proves the check exists.
- Breaking the *guarded stage* proves the check bites.

A11's M-A11-1 is the model: it appended `bevy_ecs` to
`crates/orrery_conformance/Cargo.toml` - an engine entering a gated crate's
graph through the weakest insertion point - and watched `core-gates.sh` clause
1 die with a named message and exit 1. It did not edit the gate.

The failure mode this prevents is documented three times in this corpus. #417:
deleting `feed_uplink`'s `With<LocallyAuthoritative>` filter left 95 tests
green, because a different clause refused the fixture first. A7's X-C: swapping
quantize and hash at what is now
`crates/orrery_core/src/executor.rs:173-174` survived all 21 suites, because
every in-tree state was already lattice-integer. #447's M3: `entity.to_bits()`
appended to a `DiffUplink` rode into the journal past every gate and 100 tests.
In all three the check existed, ran, and was green over a broken stage.

So the required PR-body form is three lines:

```
Break:    <edit to production behaviour, by path>
Expect:   <named check> fails
Revert:   <named check> passes, git status clean
```

and a fixture whose kill has not been demonstrated is not coverage. Two
additional rules, both earned here:

1. **Vacuity self-check.** If a fixture can pass by accident - an on-lattice
   value, an unreachable state - it must assert its own non-vacuity. PR-1 did
   this (`quantize_pin.rs:191` asserts the fixture is genuinely off-lattice);
   copy the pattern.
2. **Name the *other* check that must stay green.** S1.a's detector is only
   meaningful because `chains_match_the_committed_golden` survives the same
   mutation. A kill with no survivor named may just be duplication.

## 8. Lane partition: why these can run in parallel

Lanes run concurrently, so the partition must be by file, not by intention.
The disjointness that matters, and the two places it is tight:

| Lane | Owns |
|---|---|
| S0.a | root `Cargo.toml` `[profile]`; `crates/orrery_conformance/tests/overflow_profile.rs` |
| S0.b | `crates/orrery_games/src/skirmish/**` |
| S0.c | `docs/adr/0021-ruleset-distribution.md` |
| S1.a | `crates/orrery_games/src/{golden.rs,scenario.rs}`; `crates/orrery_games/tests/battery.rs` |
| S1.b | `crates/orrery_witness/tests/**` |
| S1.c | `crates/orrery_conformance/{src/corpus.rs,corpus/golden.json}` |
| S1.d | `crates/orrery_persistd/src/migration.rs`; `crates/orrery_persistd/goldens/**` |
| S1.e | `gates/migration-bench/**`; `scripts/check.sh`; `docs/plans/baselines/**` |
| S2 | `scripts/core-gates.sh` |
| S3.a | `crates/orrery_persist_client/src/feed.rs` **or** `crates/orrery_protocol/src/gateway.rs`, per the owner's half |
| S3.b | `crates/orrery_predict/tests/**` |
| S3.c | new `tests/ui/**` at the registration seam |
| S3.d | digest constants in `crates/orrery_games/src/{regolith,skirmish}/mod.rs` |
| S4.1 | new composition crate; new registry file |
| S4.2 | `crates/orrery_games/src/regolith/mod.rs` |
| S5 | new host crate |
| S6.a/b/c | one driver file each |

**Tight spot 1: `crates/orrery_games`.** S0.b, S1.a, S3.d and S4.2 all live in
it. Three of the four are disjoint at file granularity - `skirmish/**`,
`golden.rs` plus `scenario.rs`, `regolith/mod.rs` - with one constraint:
**S0.b must not touch `crates/orrery_games/tests/battery.rs`**, which S1.a
owns.

The fourth is a genuine collision and is not papered over: **S3.d and S4.2
both edit `crates/orrery_games/src/regolith/mod.rs`** - S3.d replaces the
digest constant at `:253-256`, S4.2 rewrites the delegation. They are two
lines apart in intent and one file apart in git. They must not run
concurrently. Since S3.d is blocked on OD-22 and S4.2 is blocked on S1.a plus
S4 lane 1, the ordering falls out on its own in most futures; where it does
not, **S3.d goes first**: it is a two-constant change, so rebasing it onto a
restructured module is cheap, whereas rebasing a module split onto a changed
constant forces the whole split back through review. `battery.rs`'s `game_test!` macro (`:26-37`) generates one test per
*property* iterating the game catalogue, not one test per game, so a Skirmish
assertion has a natural home in `skirmish/state.rs`'s own test module and no
business in the battery. This is stated in S0.b's issue as a constraint, not
left to discovery.

**Tight spot 2: `scripts/`.** S1.e edits `scripts/check.sh` (one `WORKSPACES`
row) and S2 edits `scripts/core-gates.sh`. Different files; note that
`check.sh:658` self-tests the `WORKSPACES` table against discovered
directories, so S1.e's row and its new directory must land in one commit.

## 9. Strongest argument against this sequencing

**This programme proposes to spend its first week inside the exact four trees
#329 needs frozen, for a migration whose first behaviour-changing stage has no
consumer and no date.**

The project's real critical path is #386 -> #387 -> #329 -> P4 exit. #395 says
so itself ("Not on the P4 critical path"). #329's blocker B3 asks for the tree
to be quiet before the window opens and names three lanes that must "land or be
abandoned". S1.a lands in `crates/orrery_games`; S1.b lands in
`crates/orrery_witness`; S0.b lands in `crates/orrery_games`; S4.2 lands in
`crates/orrery_games/src/regolith/mod.rs`, which has taken 10 commits in three
days and is where the campaign work lives. Every one of those is a new lane in
a frozen path, filed by a programme that has explicitly declared itself
off the critical path and is now asking to sit on it. A disciplined owner
would say: declare the quiet point today, fly the 25 hours, exit P4, and do
all of this afterwards - the fixtures pin behaviour that is not moving until
S4, and S4 has no date. Waiting costs the programme nothing it can name.

**The reply, and it is narrower than I would like.** The waiting argument is
correct for every lane except one. S1.b, S1.c, S1.d, S1.e, S2, S3.b and all of
S4-S6 lose nothing by waiting; S0.a and S0.c touch no digest tree at all. The
exception is S1.a, and it is an exception of kind rather than degree: outcome
goldens must commit *legacy* behaviour, and legacy behaviour in
`crates/orrery_games` is moving weekly - version 16, two golden regenerations,
ten commits in three days. Delay does not postpone S1.a; it changes what S1.a
is about, and the parity argument every later stage leans on gets weaker
rather than merely later. One lane, one to two days, in the tree with the
smallest queue, before the quiet point - that is the whole ask, and the
honest version of this plan asks for that and not for the rest.

**What the reply does not defeat.** If the owner would rather declare the
quiet point this week and accept that S1.a pins v17-or-whatever afterwards,
that is coherent and cheap, and the cost is one that shows up much later as a
weaker parity claim rather than as a failure anyone can point at. This node
keys the decision; it does not force it. And if the answer is "the whole
programme waits", the correct action is not to file the issues and let them
rot - it is to file them and park the digest-tree ones explicitly, which is
why every child in section 5 carries its digest-tree flag on its face.

**A second objection, weaker but real.** S4 and S5 build a composition root
and a host seam for a modularity problem that A1 measured absent (four crates,
erased at one boxed closure) and A3 could not evidence at scale. D42 clause
(b) accepted them anyway on the argument that they pay under every future -
including "never migrate" - and that argument is the record's, not mine. But
it means S4-S5 are the only stages in this plan whose value rests on an
argument rather than a measurement, and the pre-registered reversal (A11's
E-1-class experiment; the H1 pivot) should be treated as live rather than
ceremonial. If S4 lane 2 finds the two Regolith domains do not separate
cleanly, that is data, and it should be reported as data rather than solved
by widening the module interface until it fits.

## 10. What could not be verified

- **Whether the owner has decided (f)(4) out of band.** The code saturates;
  the record says the choice is owed. I read the ADR, #441, #447 and #395's
  comments and found no recorded choice. If it was made in conversation, the
  repair is a line in D43, not this document.
- **Whether replicon's registration API admits an `EngineHandleFree` bound
  without forking the vendored copy.** A9 section 9 could not confirm it and
  neither could I without building; S3.c's spike is scoped to answer exactly
  this and nothing else.
- **The cost of S4 lane 2 in merge conflicts.** I can count commits into
  `crates/orrery_games/src/regolith/` (10 in three days) but not predict how
  many land during a two-day lane. The entry condition is stated as a judgement
  ("quiet enough") because I could not make it arithmetic honestly.
- **Whether `gates/migration-bench` runs inside the current CI budget.**
  `d88b1fb3` moved slow lanes to nightly to get merges under four minutes;
  S1.e adds a `check`-role workspace, which should be cheap, but I did not
  measure it. If it is not cheap it belongs in the nightly lane, decided at
  PR time rather than discovered.
- **Everything about Unreal.** A9 marked its Unreal half unevidenced
  throughout; nothing has changed. Brief phase 8 stays deferred behind the
  owner supplying a requirement, and no stage above assumes one.

## 11. Owner-reserved

Each item is the owner's, with why. Nothing below is decided here, and no
stage above proceeds past its named blocker without it.

1. **D43 clause (f)(4): wrapping or saturating.** The record says
   implementation blocks on this and forbids canonical arithmetic that depends
   on the answer; `crates/orrery_games/src/regolith/mod.rs:1661-1673` already
   saturates. Recommended: record saturating in D43 as the decided posture
   (it is the answer the tree has already spent, and it is very likely the
   right one for a game where a clamped velocity is meaningful and a wrapped
   one is a teleport) - but the recording is the owner's act, not an
   inference from a diff. **Blocks:** nothing new; unblocks S0.b's scoping.
2. **Whether clause (f)'s flag is per-ruleset or universal.** Skirmish has no
   `arithmetic_overflowed` field. Either it owes one, or the clause is
   Regolith-scoped and should say so. **Blocks:** S0.b.
3. **OD-21, `DiffUplink.tick`.** Stamp the real tick, or rename the semantic.
   Both are small; silence is the only wrong option, and it has now been the
   answer for three days past A11. **Blocks:** S3.a, and any future design
   assuming a tick-addressed journal.
4. **OD-22, X-1's digest mechanism.** Build script, CI artifact, or lazy
   runtime hash. D49 prices them and notes a stale artifact is worse than an
   honest placeholder. **Blocks:** S3.d. Note the placeholder has begun moving
   by hand (`[0x63; 32]` -> `[0x66; 32]`), which is the drift toward a
   plausible-looking lie that D49 named.
5. **OD-26 / IV-7's enforcement mechanism.** `EngineHandleFree` bound versus
   registry-time schema refusal; D45 leaves both open and notes they are not
   exclusive. **Blocks:** S3.c and F-9, and IV-7 stays review-held until one
   lands.
6. **OD-25: N-3 granted-range derivation and id reuse after despawn.** D44
   settled reuse (stays legal, durable readers must be lifetime-aware); N-3's
   derivation remains. **Blocks:** persisting materialized entities, which no
   stage above does.
7. **OD-28: the emission cap's value and posture.** D46 says implementation of
   clause (e) blocks on the constant the way D43 blocks on (f)(4).
   **Blocks:** nothing in S0-S6.
8. **OD-24: schedule-digest and `projection_version` session assertion.** D43
   and D49 both flag the same door and both say the two flags should be taken
   or refused together. **Blocks:** nothing; out-of-band assertion suffices.
9. **The window, and what beats it.** Whether S1.a lands before the quiet
   point is a scheduling call between this programme and #329, and #329 is the
   critical path. Section 9 states the case both ways. Recommended: S1.a
   before, everything else after or window-safe. **This is the single decision
   that shapes the epic.**
10. **Whether S4-S5 proceed at all.** D42 clause (b) accepted them, so this is
    not reopening a record - it is a scheduling question about work whose
    value rests on an argument rather than a measurement (section 9's second
    objection). The pre-registered reversal conditions belong to the owner,
    and this node treats them as live.

## Cross-references

- Planning epic and traceability: [#395](https://github.com/baadc0de/orrery/issues/395)
- Implementation epic this node plans: [#626](https://github.com/baadc0de/orrery/issues/626)
- Capstone this node succeeds: [A11](a11-adrs-and-pr-plan.md) sections 2, 3, 5
- Fixture set and named checks: [A10](a10-conformance-benchmarks.md) section 9
- Source brief: [ruleset-ecs-migration-brief.md](ruleset-ecs-migration-brief.md)
- Collision findings: [collision-under-own-state.md](collision-under-own-state.md)
- Accepted records: [D42](../adr/0042-canonical-simulation-architecture.md),
  [D43](../adr/0043-determinism-envelope-and-gate-replacement.md),
  [D44](../adr/0044-identity-classes-and-allocation.md),
  [D45](../adr/0045-per-component-capability-policy.md),
  [D46](../adr/0046-message-class-semantics.md),
  [D47](../adr/0047-rollback-unit.md),
  [D48](../adr/0048-canonical-witness-projection.md),
  [D49](../adr/0049-compatibility-manifest.md)
