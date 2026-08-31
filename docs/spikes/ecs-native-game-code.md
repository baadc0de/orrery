# Spike — Should Regolith's game code be `bevy_ecs`-native?

**Propose-only. Do not merge.** Sanctioned by the owner on 2026-08-31 in
[#793](https://github.com/baadc0de/orrery/issues/793) ("Owner sanction,
2026-08-31"), as **evaluation, not adoption**. D42 (a), D42 (c) and D43 (e)(1)
are Accepted and are **not** amended by this branch. §7 names the amendments
that would be owed if the answer were yes.

---

## 0. The verdict, first

**Yes for the ergonomics, and it is a bigger win than expected. No for the
architecture, and the blocker is not the one anybody named.**

Three separable answers:

| | Verdict | Evidence |
|---|---|---|
| **Does game code get more comfortable?** | **Yes, clearly.** `speed_cap` loses two four-arm matches and two discarded zeroes; `acceleration_cap` loses its `let-else`, its `Option` handling and its respawn early-return, and becomes arithmetic with a signature. | §1 |
| **Is most of that available without `bevy_ecs`?** | **Partly, and less than the tree's own precedent suggests.** A Bevy-free projection macro recovers `speed_cap` almost fully; it recovers **nothing** of `acceleration_cap`, because the selection there is over a *pair* of samples and only components can express it. | §2 |
| **Can the three roles still agree?** | **Determinism: yes, with one new obligation. Prediction: neutral. Rollback: yes, and cheaper than feared. But D43 (e)(5) — world-of-one adjudication — is a structural blocker that no amount of measurement fixes.** | §3–§6 |

The ban's *stated* reason names three roles — game clients, field hosts,
persistd — and **none of them links `orrery_games` in a shipping build**; one of
them is not a crate that exists (§6). The ban is still defensible, but for a
crate the comment does not mention. The real cost of going native is somewhere
else entirely, and it is stated in §5.3.

---

## 1. The before and after

### `speed_cap`

**Before** — `crates/orrery_games/src/regolith/invariants.rs:42-61`. Two
`match`es over the same four variants, back to back. Two of the eight arms exist
only to produce a value the comparison then discards.

```rust
fn speed_cap(sample: &InvariantSample<'_, RegolithState>) -> Result<(), InvariantViolation> {
    let limit = match sample.current {
        RegolithState::Craft(craft) => craft.archetype.limits().max_speed_mms,
        RegolithState::Rock(rock) => rock.tier.limits().max_speed_mms,
        RegolithState::Pickup(_) | RegolithState::BloomDirector(_) => 0,
    } + VEL_MARGIN_MMS;
    let vel = match sample.current {
        RegolithState::Craft(craft) => craft.vel,
        RegolithState::Rock(rock) => rock.vel,
        RegolithState::Pickup(_) | RegolithState::BloomDirector(_) => QVel::default(),
    };
    if vel.difference_squared(QVel::default()) > i128::from(limit) * i128::from(limit) {
        Err(InvariantViolation::new(InvariantKind::SpeedCap, "regolith/speed-cap"))
    } else {
        Ok(())
    }
}
```

**After** — `crates/orrery_games/src/regolith/native.rs`:

```rust
fn speed_cap(mut findings: ResMut<Findings>, query: Query<(&Subject, &Vel, &SpeedLimit)>) {
    for (subject, vel, limit) in &query {
        let limit = limit.0 + VEL_MARGIN_MMS;
        if vel.0.difference_squared(QVel::default()) > i128::from(limit) * i128::from(limit) {
            findings.report(*subject, "regolith/speed-cap", InvariantKind::SpeedCap);
        }
    }
}
```

The two discarded arms are gone, and gone in the strong sense: pickups and bloom
directors carry no `Vel` component, so there is no place to write a zero and no
entity for the query to visit. **The set of entities the check applies to is the
query's type**, and it is stated once.

### `acceleration_cap`

**Before** — `invariants.rs:62-83`. The check applies to exactly one variant
*pair*, so the body opens by throwing everything else away:

```rust
fn acceleration_cap(sample: &InvariantSample<'_, RegolithState>) -> Result<(), InvariantViolation> {
    let (Some(RegolithState::Craft(previous)), RegolithState::Craft(current)) =
        (sample.previous, sample.current)
    else {
        return Ok(());
    };
    if previous.hull == 0 && current.hull > 0 {
        return Ok(());
    }
    // ...the actual arithmetic, eight lines later
```

**After**:

```rust
fn acceleration_cap(
    mut findings: ResMut<Findings>,
    query: Query<(&Subject, &PrevVel, &Vel, &Elapsed, &AccelLimit, &SpeedLimit), Without<Respawned>>,
) {
    for (subject, previous, current, elapsed, accel, speed) in &query {
        let per_tick = accel.0 / TICKS_PER_SEC
            + speed.0 * DRAG_PER_SEC_PER_MILLE / (1_000 * TICKS_PER_SEC)
            + VEL_MARGIN_MMS;
        if checks::exceeds_acceleration(previous.0, current.0, elapsed.0, per_tick) {
            findings.report(*subject, "regolith/acceleration-cap", InvariantKind::AccelerationCap);
        }
    }
}
```

Three separate discards collapse into the signature:

| The shipped body's line | Becomes |
|---|---|
| `let (Some(Craft(previous)), Craft(current)) = … else { return Ok(()) }` | `&AccelLimit` — only craft carry one |
| the `Some(…)` half of that pattern | `&PrevVel` — absent on a first sample |
| `if previous.hull == 0 && current.hull > 0 { return Ok(()) }` | `Without<Respawned>` |

The body is now the arithmetic and nothing else. This is the site the owner
named, and it is the site where the win is largest.

### `teleport` — the site that does *not* get better

Included on purpose. Its shipped five-arm `match (previous, current)` is one
check with three caps plus a discriminant-mismatch arm. The three caps collapse
into `SpeedLimit` exactly as above. **The mismatch arm does not collapse**: it
asks a question about the *pair* of samples, and a component set holds one
state, not two. The native form has to be *told* the answer by the projection
(the `KindChanged` marker) rather than deriving it, and the check that used to
be an arm of a `match` becomes a fifth system whose only job is to report a
marker somebody else set. That is not an improvement; it is the same work with
one more indirection.

### Counting it

Not lines — lines are a bad proxy — but **discards**: expressions whose only job
is to say "not this one".

| Check | Shipped | Native |
|---|---|---|
| `speed_cap` | 2 arms yielding discarded values, in 8 arms total | 0 |
| `acceleration_cap` | 1 `let-else` + 1 early return | 0 |
| `teleport` | 1 early return + 1 unreachable-kind arm | 1 (moved into the projection) |

---

## 2. The control arm: how much of this is ECS, and how much is just projection?

This is the question a spike that only built the native version would have
failed to ask. `crates/orrery_games/src/regolith/mod.rs:6-31` already carries
`projected_system!`, described in its own comment as *"the projection half of an
ECS query, and deliberately only that half"*. It deleted the four-way
`match own { .. }` that used to open `Ruleset::step` — with no `bevy_ecs`
anywhere. `invariants.rs` simply never got the same treatment.

`native::control` gives it that treatment: a `Moving` trait implemented for
`Craft` and `Rock`, plus an `over_moving!` macro that writes the variant list
once.

**Result, split by site:**

- **`speed_cap`: the control recovers nearly all of it.** Eight arms become two,
  the discarded zeroes are gone, and the check body is the arithmetic. What
  remains is that the variant list is still written *somewhere in the rules
  crate* — adding a fifth entity kind is an edit to a `match`, where natively it
  is a decision about which components the kind carries.
- **`acceleration_cap`: the control recovers essentially nothing.** The
  `let-else` survives verbatim. A projection macro selects on `current`; this
  check selects on the *pair* `(previous, current)`, and no macro over a sum
  type expresses that as a signature. Only components do — `&PrevVel` present or
  absent is a fact about the pair, held as a fact about the entity.

So the honest split is: **the enum-dispatch half of the discomfort is a
projection problem the tree already knows how to solve without Bevy. The
sample-pair and exemption halves are genuinely ECS-shaped, and the control arm
cannot reach them.**

All three arms are differenced against each other on one corpus in
`crates/orrery_games/tests/ecs_native_invariants.rs` (13 rows: all four
variants, both directions of every check, the first-sample case, the respawn
edge, and a kind change). They agree.

One behavioural difference surfaced and is recorded rather than papered over:
`orrery_core::evaluate` **short-circuits on the first failing check**, while a
native pass runs every system and reports all of them. The differential asserts
containment, not equality, and says so at the assertion.

---

## 3. Axis 1 — Determinism

**`tier_h_projection_differential.rs` still passes.** Unchanged, on this branch,
both of its tests:

```
test the_canonical_schedule_composes_unambiguously_and_the_unordered_mutant_does_not ... ok
test permuted_insertion_orders_agree_on_the_sorted_projection_and_the_executor_chain ... ok
```

That was expected — the branch does not touch `orrery_sim_host`'s schedule. The
open part of the axis, as #793 states it, is *query iteration order once game
code iterates directly*. That was measured, and the answer is:

**Query iteration order is not deterministic across permuted insertion, and the
native form makes canonicalizing it the game author's job.**

`ecs_native_invariants.rs` discharges this in the same two-directional shape the
Tier-H differential uses:

- `permuted_insertion_orders_yield_equal_sorted_findings` — three insertion
  permutations, equal canonicalized output. Passes.
- `some_permutation_disagrees_on_the_unsorted_findings` — at least one pair of
  permutations must **disagree** on query-visit order. Passes, which is what
  makes the sort a measured necessity rather than a precaution.

The cost is small and the obligation is real. Today the host iterates in
`PersistId` order on the rules' behalf (`ecs.rs:289`, `order.sort_by_key(|slot| slot.persist)`), so a rule *cannot* observe archetype order. A rule holding its
own `Query` can, and nothing in the type system stops it. This converges with
what #787 already named as blockers 2 and 3, and #793's own reading is right:
that work is owed under either architecture.

**Not a blocker. A new, permanent, unenforced obligation on every rules author.**

---

## 4. Axis 2 — Prediction

`docs/05-prediction-rollback.md` §1 and §3, and `orrery_predict/src/config.rs:61`
(`rollback_ticks: 9`).

**Finding: neutral on the numbers, and the interesting part is not a number.**

D8's arithmetic — 60 Hz tick, 20 Hz send, 100 ms interpolation, 16-tick ring —
is indifferent to whether the rules are a trait or a schedule. Nothing measured
here moves it.

What is *not* neutral is where the prediction world lives. docs/05 §3 is
explicit that the point of the design is **"snapshotting only the predicted
subset — never the world … what makes cost scale with interest size instead of
world size"**. The client already runs a Bevy app with lightyear's prediction
stack in it. The obvious prize of native rules on the client is that prediction
and canonical rules would share one world, one set of components, one
`Transform` — no marshalling.

**That prize is exactly what D42 (c) rejects, permanently and with no reversal
condition**: *"Hosting canonical state in a world shared with presentation and
replication state is rejected as an architecture, permanently … Rejected
outright means: no trigger, no pilot, and no reversal condition in this record
reopens it."*

So a Bevy-native ruleset that keeps D42 (a)'s dedicated world buys the
prediction path **nothing at all** — the same per-entity marshalling between the
app world and the rules world happens either way, and now there are two Bevy
worlds instead of one Bevy world and one `BTreeMap`. The prediction benefit of
going native is available only by breaching a clause that is closed to
amendment.

This was not obvious before the spike and is, on its own, most of the answer.

---

## 5. Axis 3 — Rollback

The sharp one, and the one the brief asked to prioritise. Two measurements and
one structural finding.

### 5.1 The 9-tick window on the two existing substrates

`crates/orrery_sim_host/tests/spike_793_rollback_cost.rs`. Full Regolith rules,
identical sealed inputs, snapshot → restore → 9-tick resim, release build, best
of nine. **Both backends are asserted to produce an identical canonical chain
and an identical post-rollback projection**, which they do at every population.

```
#793 rollback cost — 9-tick window, best of 9, microseconds

     N    alive  Executor (snap/rest/resim)  EcsBackend (snap/rest/resim)     ratio
    32       32       0.3      1.0    171.7       0.9      3.2    179.4      1.06
   128      128       1.1      5.9    696.9       4.2     13.7    714.3      1.04
   512      512      18.8     56.2   2825.7      19.6     74.6   2830.1      1.01
```

```
#793 per-tick resim cost vs D8 budget (step_cost target 1 ms)

     N  Executor µs/tick       Ecs µs/tick  full window / 5 ms frame
    32             19.28             19.55                   0.036
   128             77.78             78.65                   0.145
   512            316.34            310.02                   0.577
```

Read against `RollbackBudget`'s defaults (`step_cost` ≈ 1 ms,
`max_resim_per_frame` = 5 ms, `max_amortize_frames` = 2): a full 9-tick window
over **512 entities** costs 58 % of *one* frame's resim budget on this machine.
A plausible predicted subset — docs/05 §1 puts the high-rate interest set at 24
— is two orders of magnitude inside the guard. **The ECS substrate costs between
1 % and 6 % more than the `BTreeMap`, and the gap narrows as the population
grows.**

The resim cost is not the problem. That is worth saying plainly, because it was
the thing the issue expected to be the problem.

### 5.2 What a *native* tick costs, and what a native restore costs

The measurement above hosts state in components but still calls
`canonical_step` once per entity out of a `for` loop. That is not what native
means. `crates/orrery_games/tests/spike_793_native_schedule_cost.rs` measures
the part that is genuinely new: dispatching a chained schedule over a populated
world, nine times.

```
#793 native schedule dispatch — 5 chained systems, 9-tick window, best of 9, microseconds

     N       world build    1 schedule run   9 runs (window)
    32             26.83              0.30              2.52
   128             45.63              0.81              7.21
   512            110.52              2.92             25.98
```

Two readings:

- **Schedule dispatch is cheap.** 2.9 µs for one run over 512 entities; 26 µs
  for the whole 9-tick window. `bevy_ecs`'s per-run overhead is not what will
  break D8's budget.
- **World construction is not.** Building the 512-entity world costs 110 µs —
  **4.3× the entire rollback window it enables.**

That second number is the finding. `bevy_ecs` 0.19.1 has **no
`impl Clone for World`** (verified by grep over the crate's own
`src/` in the cargo registry: zero hits for `impl Clone for World`), so
there is no container copy to take. A rollback restore is one of two things:

1. **Restore in place**, writing components on entities that already exist. This
   is what `EcsBackend`'s `insert_observed` does, and §5.1 measures it: 74.6 µs
   at N=512, 1.33× the `Executor`'s 56.2 µs. Acceptable.
2. **Rebuild**, when the population changed during the rolled-back window. Then
   entities must be spawned and despawned, archetypes move, and the cost is the
   110 µs figure.

Regolith's population *does* change mid-window — splits materialize rocks,
blooms seed them, rocks drop pickups; `Ruleset::materialize` at `regolith/mod.rs:641` is
the path. So case 2 is the normal case, not the pathological one.
It is still comfortably inside D8's budget at these populations, but it is the
term that grows, and it is a term the `BTreeMap` does not have.

### 5.3 The structural finding, which is the actual blocker

D43 (e)(5), Accepted and unamended:

> exposes single-entity step semantics to witnesses and adjudication: the
> verdict must hold **in a world of one**, and "the schedule was deterministic"
> is never a substitute for per-entity replay.

`Ruleset::step` takes `&mut StateView<'_, CoreState>` — **one entity's own state
plus a recorded, staleness-bounded neighbour view** (`orrery_core/src/ruleset.rs:338`,
`StateView` at `:102`). Every neighbour read is logged, because
`StateView::neighbor` takes `&mut self` precisely so that reading has a side
effect on the log (`ruleset.rs:176`). That signature is what makes
world-of-one replay *possible* — not a convention, a type.

`Query<&mut Rock>` deletes that guarantee. Not as a bug: **deleting it is the
entire point of a query.** A system that iterates the population can reach any
entity in it, without recording that it did, and there is no signature that says
otherwise. A native ruleset can be written to preserve world-of-one — the spike's
five systems do, and
`ecs_native_invariants.rs::the_native_pass_agrees_on_every_row_individually`
proves it row by row — but they do so **because they were written to**, and the
ECS gives no help whatsoever in keeping them that way. The next author who
writes the obvious pairwise collision system between two rocks has silently made
the adjudicator's verdict depend on the population, and nothing fails.

This is not a cost that measurement can retire. It is a guarantee that is
currently structural and would become conventional.

---

## 6. The ban's stated reason — tested, and it does not hold as written

`scripts/core-gates.sh:220-222`:

> The same build links into game clients, field hosts and persistd. A Bevy
> dependency would not just be weight — it would make those three disagree,
> which is the exact failure the crate exists to detect.

That sentence is **copied, near-verbatim, from `crates/orrery_core/Cargo.toml:8-12`**,
where it is a statement about `orrery_core`:

> The same build links into game clients (witness-side re-execution),
> `orrery_field_host` (parked-cell catch-up) and `persistd` (adjudication) …

It is true of `orrery_core` — `persistd` does list `orrery_core`
(`orrery_persistd/Cargo.toml`), and the client links it too. **It is not true of
`orrery_games`.** The three named roles check out as follows:

| Named role | Links `orrery_games`? |
|---|---|
| game clients | **No.** `crates/orrery/Cargo.toml` lists `orrery_games` under `[dev-dependencies]`, not `[dependencies]`. The shipping client does not link the rules crate at all. |
| field hosts | **No such crate exists.** `crates/` contains no `orrery_field_host`; the name appears only in `orrery_core/Cargo.toml`'s comment, as a future consumer. |
| persistd | **No.** `crates/orrery_persistd/Cargo.toml` does not list `orrery_games` in any section, and the only occurrence of the string anywhere in its sources is a comment at `intent/mod.rs:3952`. |

So none of the three roles the gate names links `orrery_games` in a shipping
build.

Parsing every workspace manifest for the dependency gives the complete picture:

| Crate | `[dependencies]` | `[dev-dependencies]` |
|---|---|---|
| `orrery` (client) | — | ✓ |
| `orrery_sim_host` (the seam) | — | ✓ |
| `orrery_coordinator` | — | ✓ |
| **`orrery_sim`** | **✓** | — |

**`orrery_sim` is the only non-dev consumer of `orrery_games` in the entire
workspace.** It is `crate-type = ["rlib", "cdylib"]`, has exactly three
dependencies, and exposes a `#[repr(C)]` C ABI over headless Regolith
(`orrery_sim/src/lib.rs:1-5`). A `bevy_ecs` dependency inside a cdylib exported
to C is a genuine problem — and this crate is the one the gate's comment does
not mention.

**Conclusion on this point.** The ban is defensible: `orrery_sim` is a real
Bevy-free consumer, and the *future* consumers the record contemplates (a field
host, witness-side re-execution linking rules directly) would be more. But its
stated justification is inherited verbatim from the crate above it and is
**wrong about all three roles it names as they stand today**. That is worth
correcting on its own account: a gate whose comment misdescribes which build
breaks is a gate nobody can reason about when it fires.

The #793 body's framing — "the set of contexts that genuinely need a Bevy-free
ruleset is smaller than when the ban was written, and enumerating it honestly is
a precondition for this decision" — is correct, and the enumeration is the table
above.

### The load-bearing unknown, addressed

#793 asks whether #770's generalisation from *hosting the executor's state* to
*hosting Bevy-native rules* holds. It does not, and the reason is the amendment
text at D43 (e)(5) rather than anything measurable: the adjudicator re-executes
on the ECS substrate today **because both backends call
`orrery_core::canonical_step` and neither copies it** — the record says outright
that "the two substrates are byte-indistinguishable to the adjudicator by
construction". Native rules remove that shared call site. There is then no single
place where a canonical byte is produced, and the observational guard D43 (e)(5)
was closed with (a counting backend, a swapping backend, a width probe) has
nothing to observe. **`ReplayHarness` being generic over `TickBackend` does not
generalise to Bevy-native rules; it generalises over two hosts of one rule
implementation.**

---

## 7. Gates: what fails, and what is owed

### What fails

`./scripts/core-gates.sh` exits **1**, with exactly one line:

```
core-gates: orrery_games has Bevy in its dependency graph
```

That is clause 1, by name, at `core-gates.sh:225`. It is intended, it is not
worked around, and **`core-gates.sh` is not edited on this branch.**

The dependency is deliberately a plain, non-optional one
(`crates/orrery_games/Cargo.toml`). Putting it behind a non-default feature
would have made `cargo tree -p orrery_games` — which is exactly what the gate
greps — keep reporting green while Bevy was in the crate. A spike that hides its
own violation has measured nothing.

### Confirming nothing *else* broke

Clause 1 aborts the script, so the rest of the battery is unreachable while the
dependency is present. To check it, the dependency line was commented out, the
gate run, and the line restored immediately (the source file
`regolith/native.rs` stays in the tree either way, so the source-scanning clauses
still see it). With Bevy off, everything passes:

```
core-gates: Bevy-free: orrery_conformance orrery_core orrery_games
core-gates: async-runtime-free: orrery_conformance orrery_core orrery_games
core-gates: role discovery: orrery_conformance orrery_core orrery_games
core-gates: VC-4: no unordered collections
core-gates: VC-6/VC-8: no ambient inputs, no std transcendentals
core-gates: recorded neighbour reads confined to 1 declared audited predicate(s)
core-gates: Tier-H host allowlist: orrery_sim_host
core-gates: Tier-H hosts depend on bevy_ecs only: orrery_sim_host
core-gates: Tier-H source battery (VC-4/VC-6/VC-8 + async + RNG construction) over 2 host source(s)
core-gates: Tier-H harnesses declared and present: 6
core-gates: verifiable-core static gates pass
```

So: **the Bevy-free clause is the only clause the spike breaks.** VC-4, VC-6,
VC-8, the neighbour-read confinement and the whole Tier-H battery are green with
the new sources in the tree.

`./scripts/check.sh fmt` and `./scripts/check.sh clippy` both pass. The test
lane's result is recorded in the PR.

### The amendments that would be owed

Named here, not made — the way #745 named D42 (d).

1. **D42 (a)** — *"Canonical verifiable state stays in the engine-neutral
   per-entity executor."* Already amended once (2026-08-31) to read the *seam*
   rather than the container as normative, on the grounds that "every committed
   byte is produced by `orrery_core::canonical_step`, which both backends call,
   and `orrery_core` carries no Bevy dependency." **Native rules break that
   sentence's premise**, because the rules would produce their own bytes. This
   is the deepest amendment owed and it is not a wording change.
2. **D43 (e)(1)** — the Tier-H host allowlist, and with it the clause 1 crate
   scan. `orrery_games` would have to be admitted as a host, which means the
   whole Tier-H battery (canary, projection differential, world-of-one,
   adjudication substrate) applies to the *rules crate* and not just to the
   seam.
3. **D43 (e)(5)** — world-of-one. §5.3 argues this one cannot be amended by
   wording: a native ruleset either keeps the property by convention or gives it
   up. If the owner wants native rules, this clause has to be either re-derived
   on a new mechanism or downgraded, and downgrading it is downgrading what an
   adjudication verdict means.
4. **D42 (c)** — **not** owed, and worth stating: nothing on this branch touches
   the application-world rejection, and §4 argues the prediction-side prize of
   going native is unreachable *because* (c) holds.
5. **`core-gates.sh:220-222`'s comment** — owed regardless of this spike's
   outcome. All three roles it names (game clients, field hosts, persistd) fail
   to link `orrery_games` today, and it omits `orrery_sim`, the crate that
   actually does and is a cdylib with a C ABI. Not an ADR amendment — a comment
   correction — but the gate is unreasonable-about when it fires until it is
   made.

---

## 8. What broke, and what could not be tested

**What broke:**

- `clippy::type_complexity`, at `-D warnings`, refuses the query signatures that
  carry the ergonomic win — twice, on `acceleration_cap` and `teleport`. The
  standard `#[allow]` is applied and annotated in the source. It is not free:
  the lint that would catch a genuinely tangled type in Regolith is now off for
  those functions. Adopting this style means an `allow` per system or turning the
  lint off crate-wide. **This is a real, recurring ergonomic tax that partly
  offsets the win in §1.**
- `World::entities().len()` is not the population: `bevy_ecs` 0.19 backs
  component and resource ids with entities of its own, so it read roughly 2× on
  a fresh world. Counted via a `Subject` query instead. A small thing, but the
  kind of thing every rules author will hit once.

**What could not be tested, and why:**

- **The full rules were not ported.** Regolith is 6 199 lines across ten
  files — `mod.rs` (2 232), `state.rs` (936), `order.rs` (940), `craft.rs`
  (513), `visibility.rs` (438), `world.rs` (391), and four smaller ones. §5.2's schedule-dispatch figure is therefore a **floor**, not
  a measurement of native Regolith. A real native Regolith pays that plus its
  arithmetic — but the arithmetic is the same arithmetic either way, so the floor
  is the part that is genuinely new.
- **The persistd / field-host agreement question could not be tested by
  building it**, because there is nothing to build: persistd does not depend on
  `orrery_games` and no field-host crate exists (§6). The question is answered by
  enumeration rather than by experiment, and the enumeration is checkable.
- **No cross-platform determinism run.** The determinism matrix was not
  exercised; `bevy_ecs`'s archetype layout across targets is untested here. §3's
  canonical sort makes the *output* order independent of layout, but nothing on
  this branch proves a `bevy_ecs` build is identical across the matrix, and
  that is precisely the property `orrery_games`'s Cargo comment claims for the
  crate today.
- **`ResimPlan` / `RollbackBudget` were read, not driven.** §5.1 reports raw
  costs against the guard's documented thresholds; no test constructs a
  `RollbackBudget` and asks it to plan. That would be worth doing if this ever
  goes past a spike.

---

## 9. Recommendation

**Do not adopt. Do three of the four things the spike found anyway.**

The ergonomic case is real and better than the issue expected — §1 is not a
close call, and §2 shows a meaningful part of it is *not* reachable without
components. If the only question were "is the game code nicer", the answer is
yes.

But the win is confined to stage-1 checks: `invariants.rs` is 302 lines of
Regolith's 6 199, and it is bought with D43 (e)(5) — the property that makes an
adjudication verdict mean something. Trading a structural guarantee for
ergonomics in the crate whose entire purpose is being trustworthy is the wrong
trade, and §4 shows the one place where going native would pay for itself
architecturally (a single client world) is closed by D42 (c) with no reversal
path.

Worth doing regardless of this decision, in ascending cost:

1. **Correct `core-gates.sh:220-222`.** All three roles it names fail to link
   `orrery_games`; the one that does — `orrery_sim`, a cdylib with a C ABI — is
   not mentioned. Cheap, and the comment is currently misleading about which
   build a violation would break.
2. **Give `invariants.rs` the `projected_system!` treatment.** §2's control arm
   recovers most of `speed_cap`'s discomfort with a macro, a trait and no
   dependency. Bounded, reversible, and it is the tree's own existing idiom.
3. **Do #787's blockers 2 and 3.** #793's own suggested next step, and §3
   confirms it: result and event order becoming enforced sorts is owed under
   either architecture.
4. If native rules are ever revisited, **start at D43 (e)(5)**, not at
   ergonomics. Everything else here is affordable; that one is not obviously
   payable at all.

---

## Reproducing

```sh
# the differential — three arms, one corpus, and the order obligation
cargo test -p orrery_games --test ecs_native_invariants

# the rollback measurement (§5.1)
cargo test -p orrery_sim_host --release --test spike_793_rollback_cost -- --nocapture --test-threads=1

# the native schedule floor (§5.2)
cargo test -p orrery_games --release --test spike_793_native_schedule_cost -- --nocapture --test-threads=1

# determinism, unchanged (§3)
cargo test -p orrery_sim_host --test tier_h_projection_differential

# the gate, failing by name (§7)
./scripts/core-gates.sh; echo "exit=$?"
```

## Files

| Path | What |
|---|---|
| `crates/orrery_games/Cargo.toml` | the deliberate `bevy_ecs` dependency, and why it is not behind a feature |
| `crates/orrery_games/src/regolith/native.rs` | the native components, the five systems, and the Bevy-free control arm |
| `crates/orrery_games/tests/ecs_native_invariants.rs` | three-arm differential + the two-directional order obligation |
| `crates/orrery_games/tests/spike_793_native_schedule_cost.rs` | §5.2 |
| `crates/orrery_sim_host/tests/spike_793_rollback_cost.rs` | §5.1 |

Reverting the whole violation is: delete `native.rs`, the three test files, and
the `bevy_ecs` line from `crates/orrery_games/Cargo.toml`.
