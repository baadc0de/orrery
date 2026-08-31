# Spike — How should a native ruleset reach a neighbour?

**Propose-only. Do not merge.** Follows
[#796](https://github.com/baadc0de/orrery/pull/796), whose verdict was
"ergonomics yes, architecture no, and the blocker is D43 (e)(5)". This branch
takes that blocker apart. Sanctioned under
[#793](https://github.com/baadc0de/orrery/issues/793) as **evaluation, not
adoption**. D42 (a), D43 (e)(1) and D43 (e)(5) are Accepted and are **not**
amended here. §8 names what would be owed.

Branch: `spike/neighbour-access-options`. Code:
`crates/orrery_games/src/regolith/neighbour_options.rs`. Evidence:
`crates/orrery_games/tests/spike_793_neighbour_options.rs` (10 tests, all
passing).

---

## 0. The verdict, first

**A wins. It is not close, and the reason is not that A is cheap.**

Three findings carry it, and the first two shrink the problem before the
options are even compared:

| | Finding | Evidence |
|---|---|---|
| **1. The collision is one call site.** | `InvariantSample` carries no neighbours, so **#796's entire ergonomic win involves zero neighbour reads.** The workspace has exactly one production `StateView::neighbor` call. | §1 |
| **2. The tree already made this decision, and made it as a type.** | `sched.rs` splits an ECS query into *selection* and *projection*, takes the projection, refuses the selection, and enforces it by signature: `Observation` holds the `StateView`, `System` has nothing to read a neighbour *from*. The four options are groping toward a factoring that exists. | §2 |
| **3. A recorded read is a *lookup*, not a *dereference*.** | Every query-shaped option separates finding a neighbour from reading its payload, and the split is where the guarantee escapes. B leaks absence with an empty log. **This is the finding that kills B.** | §4 |

The one thing the brief asked for that A does not have — a defence of the
enumeration gap — turns out to be already answered by the architecture, in a
way that predates the question: Regolith **already enumerates**, deliberately,
*outside* the step, and launders the result into a named input (§3). The gap is
real, it matters, and D is the wrong way to close it.

A fifth option not in the brief — **E, registration-time refusal** — is the only
mechanism that would restore a total guarantee if native ever were adopted. It
is built and it works (§6). It is not a reason to adopt native.

---

## 1. The problem is one call site, and it never wanted a query

### 1.1 The ergonomic win has no neighbours in it

`crates/orrery_core/src/invariants.rs:67-79`:

```rust
pub struct InvariantSample<'a, S> {
    pub entity: PersistId,
    pub current: &'a S,
    pub tick: Tick,
    pub previous: Option<&'a S>,
    pub elapsed_ticks: u32,
}
```

Own state and own history. Nothing else. Every site #796 improved —
`speed_cap`, `acceleration_cap`, `teleport` — is a function of this struct, so
the entire measured ergonomic win is **projection over one entity's own state**
and reads no neighbour at all.

### 1.2 There is one production neighbour read

Workspace-wide, `\.neighbor\(` returns 17 hits. Fourteen are
`CellId::neighbor(IVec3)` — a spatial-lattice method in `orrery_protocol`,
`orrery_spatial` and `orrery_persistd`, unrelated. Of the three
`StateView::neighbor` families, `orrery_core/src/ruleset.rs:426-444`,
`orrery_core/src/executor.rs:1082-1318` and
`orrery_core/tests/adjudication.rs:152` are test modules.

**Production: `crates/orrery_games/src/regolith/visibility.rs:171`, and nothing
else.** `Skirmish` declares `max_neighbor_reads() == 0`
(`crates/orrery_games/src/skirmish/mod.rs:111`).

### 1.3 That call site asks by name, from the inputs

```rust
// crates/orrery_games/src/regolith/visibility.rs:169-172
let [locker, rock, collision] = NEIGHBOR_READ_SLOTS.map(|slot| {
    slot.target(cover, collision_id)
        .and_then(|id| view.neighbor(id).cloned())
});
```

`cover` is `(locker, rock)` destructured from an `Order::ClaimCover`;
`collision_id` from an `Order::Collide` (`visibility.rs:154-168`). **Both ids
arrive in the signed input log.** The rule does not ask who is nearby; it is
*told* whom to check, and its job is to check.

So: the ergonomic win reads no neighbours, and the one neighbour read is a
by-name lookup that a query's selection powers would not improve. **The
collision between `Query<&mut Rock>` and the recorded-read discipline is one
call site that never wanted a query.**

---

## 2. The tree has already answered this, as a type

`crates/orrery_core/src/sched.rs:5-19`, in the module docs, unprompted by this
spike:

> An ECS query fuses two things: **selection** over a population, and
> **projection** onto the components an operation actually touches. Orrery's
> adjudication contract forbids the first — every cross-entity read is
> recorded, capped and replayed from the log, so a rule that scanned a
> population would either emit O(N) frames per entity-tick or read outside the
> log and break replay. It forbids nothing about the second.
>
> So this module takes the projection and leaves the selection.

And `sched.rs:21-30` states the enforcement:

> Here they are kept honest by a signature: an `Observation` receives the
> `StateView` and is the only thing that can, while a `System` receives
> `&mut R::CoreState` and has no `StateView` to read a neighbour from.

Declared in `crates/orrery_core/src/sched.rs:105-141`:

```rust
pub type SystemFn<R, L>      = fn(&mut R::CoreState, &mut StepCtx<'_, R, L>);
pub type ObservationFn<R, L> = fn(&mut StateView<'_, R::CoreState>,
                                  &OrderedInputs<'_, R::CoreInput>, &mut L);
```

Regolith's schedule has exactly one `Observation`
(`crates/orrery_games/src/regolith/mod.rs:799, 818-821`), and it is
`observe_claims` — the `verify_claims` wrapper at `mod.rs:756-770`, whose own
doc says it is an `Observation` rather than a `System` precisely so that "a rule
cannot read a neighbour" is a fact about the signature.

**This is Option A, and it is not "no mechanism".** It is a deliberate, already
type-enforced split of exactly the axis the four options are about. #796's
"my systems preserve it only because I wrote them to" is true of the *native*
port and false of the shipped tree.

---

## 3. The enumeration gap — real, and already answered elsewhere

The brief's sharpest question: the recorded read-set bounds *values consulted*
and has never bounded *the shape of the population*, because nothing could
observe that shape. Does that matter?

**It matters, and Regolith already has a discipline for it that is better than
recording.**

`crates/orrery_games/src/regolith/mod.rs:568-579`:

```rust
/// Nominate the nearest contact that is worth submitting as [`Order::Collide`].
///
/// This is the deliberately untrusted broad phase shared by live input sources.
/// It reads replicated snapshots outside the canonical step and grants no state
/// change: `visibility::verify_claims` repeats the integer predicate against a
/// recorded neighbour frame before the rules apply either body's force.
pub fn collision_candidate<'a>(
    entity: PersistId,
    own: &RegolithState,
    neighbors: impl IntoIterator<Item = (PersistId, &'a RegolithState)>,
) -> Option<PersistId> { .. }
```

This **is** enumeration — `neighbors.into_iter().filter_map(..).min_by_key(..)`
at `visibility.rs:106-141`. It runs outside the canonical step, it is
explicitly untrusted, and its only product is a *proposal*: an
`Order::Collide { other }` that enters the signed input log and is then
re-verified inside the step against a recorded neighbour frame.

So the architecture's answer to "who is nearby" is already:

> **Enumerate outside the step, where the answer can only propose. Name the
> result as an input, where it is signed. Decide inside the step, where every
> read is recorded and capped.**

That is strictly stronger than recording the enumeration, because it does not
have to trust it. A dishonest broad phase nominates a bad target and the
audited predicate refuses it; nothing needs to check that the *scan* was
honest, because the scan has no authority.

**What would be new, and what actually matters,** is enumeration *inside* the
step. There, a rule could branch on population — `q.iter().count()` — and change
own state with nothing in the log. Today the step signature makes that
unwritable. That is the property worth protecting, and §5 scores the options on
whether they protect it.

---

## 4. The finding that decides it: a recorded read is a lookup

`StateView::neighbor` (`crates/orrery_core/src/ruleset.rs:176-199`) records
**before** it knows whether it found anything:

```rust
let found = fresh.then(|| self.neighbors.get(&id)).flatten();
if !self.reads.contains(&id) {
    self.reads.push(id);
}
found
```

The absent read produces a `NeighborFrame { present: false, .. }`, and replay
requires the sequence to match exactly
(`crates/orrery_core/src/replay.rs:325-327`):

```rust
if outcome.neighbor_reads != expected_reads {
    return Err(ReplayError::NeighborFramesMalformed);
}
```

**Every query-shaped option splits the lookup from the dereference.**
`query.get(e)` returns a `Result` whose *discriminant* already answers "does
this entity exist and did it replicate to me" — an existence bit, obtained
before any token is exchanged, with the log untouched.

This is not hypothetical. `spike_793_neighbour_options.rs`,
`option_b_leaks_absence_with_an_empty_log`:

```rust
let saw_it = store.contains_key(&ABSENT);
assert!(!saw_it, "the rule now knows entity 9 is not there");
assert!(log.reads().is_empty(),
        "...and nothing in the type system made it say so");
```

Under B the fix is a `read_absent(id)` line the author must remember. Under A
the same program **cannot be written**, because the lookup is the recording
call.

And the absent read is exactly the one dishonesty wants: *"I never saw the
occluder"* is the cover claim that pays.

---

## 5. The options at the real call site

The shipped read stage is `visibility.rs:169-172`, quoted in §1.3. Everything
below it — `verify_visibility`, `verify_collision`, the integer predicates — is
untouched by every option, so only the read stage varies.

### A — status quo

```rust
let [locker, rock, collision] = NEIGHBOR_READ_SLOTS.map(|slot| {
    slot.target(cover, collision_id)
        .and_then(|id| view.neighbor(id).cloned())
});
```

Three slots, one call, recording inside it, absent case included. The slot table
(`visibility.rs:41-45`) derives `MAX_NEIGHBOR_READS`, so adding a read requires
a new slot, raises the cap with it, and is a ruleset-version bump.

### B — token exchange

`neighbour_options.rs`, `read_stage_b`:

```rust
targets.map(|target| {
    let id = target?;
    match lookup(id) {
        Some(n) => log.read(n),
        None => {
            log.read_absent(id);   // ← the line a contributor may not write
            None
        }
    }
})
```

Longer, and the `None` arm is discipline again. Written correctly it records the
same sequence as A (`option_b_call_site_matches_the_baseline_when_written_correctly`).
**B's ceiling is A's floor.**

The `Neighbour<S>` id is deliberately carried *inside* the component rather than
passed alongside — the brief's `read(&mut self, id, &Neighbour<S>)` lets the
caller record one id and read another. But `id()` cannot be privileged: a query
that could not name what it matched could not hand anything to `read` either.
So B leaves the population fully observable
(`option_b_leaves_the_population_observable`).

### C — forbid iteration

`read_stage_c`:

```rust
targets.map(|target| target.and_then(|id| neighbours.get(log, id)))
```

Structurally identical to A. The recording is inside `get`, so the absent case
records itself — which is the only reason C bounds anything B does not. Note
what that makes C: *a `get` that logs, by named id, capped.* That is
`StateView::neighbor`, with a `Query` behind it and a `PersistId`→`Entity`
translation in front. The brief's own guess — "arguably `neighbor()` with more
ceremony" — is exactly right, and the ceremony is the translation table.

### D — logged enumeration

`read_stage_d` is `read_stage_c`. **D does not improve this call site at all**,
because the three targets are still named ids from the inputs. What D changes is
the record, everywhere:

- `Regolith::max_neighbor_reads()` is **3** (`visibility.rs:47`,
  `mod.rs:620-622`). `replay.rs:275-278` rejects a window carrying more frames
  than the cap. Under D the cap must rise to a population bound — **at which
  point it bounds nothing.**
- Measured on the shipped scenarios (`option_d_log_growth_on_the_shipped_scenarios`,
  `--nocapture`):

  | scenario | entities | ids yielded per entity-tick | cap |
  |---|---|---|---|
  | `duel` | 2 | 1 | 3 |
  | `island` | 8 | 7 | 3 |
  | `island-lossy` | 8 | 7 | 3 |

  Worst shipped case **2.3×** the cap. At #796's rollback-benchmark population
  of 512 the unfiltered figure is 511 against 3 — **170×**. These are per
  entity-tick, so the window multiplies by the same factor.
- A signed window stops meaning *these values were consulted* and starts meaning
  *these entities were visible, and these values were consulted* — a strictly
  larger claim the authority must substantiate, and a new class of divergence
  where a witness disagrees about visibility having agreed about every value.
- Replay stops being O(recorded reads) and becomes O(visible population): every
  yielded id is a frame `ReplayHarness` must install and take back
  (`replay.rs:308-318, 331-334`).

### E — registration-time refusal (not in the brief)

If native ever were adopted, the guarantee has to be rebuilt somewhere, because
under a native ruleset a system's capabilities are whatever the *game author*
put in the signature. `bevy_ecs`'s `System::initialize` returns a
`FilteredAccessSet` (`bevy_ecs-0.19.1/src/system/system.rs:162`), so "this
system can reach a neighbour" is a decidable property of a built schedule —
the same shape as the ambiguity canary the host already runs
(`crates/orrery_sim_host/src/ecs.rs:528`, `audit_ambiguity`).

`audit_neighbour_access` refuses any system whose combined access reads
`Neighbour<S>` without writing `ReadLog`. It works
(`option_e_refuses_a_neighbour_reader_that_does_not_hold_the_log`), and it is
**build-time, not compile-time** — total over the registered schedule, which is
more than B, C or D manage.

Its limit is asserted, not asserted-in-prose
(`option_e_cannot_prove_the_log_was_used`): E proves a system *holds* the log,
never that it *used* it. It composes with B; it does not replace it.

---

## 6. Scoring

| | **A** status quo | **B** token | **C** no iteration | **D** logged enum. | **E** registration audit |
|---|---|---|---|---|---|
| **World-of-one replay** (`tier_h_world_of_one`, `tier_h_adjudication_substrate`) | **Pass** — 6/6, unchanged | Pass *if* absent arm written | Pass | Pass only after the cap and the record are amended | Pass (orthogonal) |
| **Type or convention?** | **Type.** `System` has no `StateView` | **Convention** for absence — the hole | Type, *because* `get` logs | Type for values; the yielded set is a new record obligation | **Build-time**, total over the schedule |
| **Ergonomics at `visibility.rs:171`** | 4 lines, one call | ~10 lines, two arms | 1 line + a `PersistId`→`Entity` table | Same as C — **no gain** | n/a (not a call-site mechanism) |
| **Signed window meaning** | values consulted | values consulted | values consulted | **visible set + values** — larger claim, new divergence class | unchanged |
| **Log growth** | — | — | — | **2.3× shipped, ~170× at N=512** | — |
| **Enumeration leakage** | **Closed** — nothing to enumerate | **Open** — count, filter, order, all unlogged | **Closed** — no `iter()` | Closed by recording it | Closed *if* iteration is refused too |
| **Replay cost** | O(recorded reads), ≤3 | O(recorded reads) | O(recorded reads) | **O(visible population)** | unchanged |
| **#787 canonical-order obligation** | none | none | none | **Yes** — the yielded set must be canonically ordered or the record becomes insertion-order-dependent | none |
| **New mechanism** | none | component + log + privacy | component + wrapper + id index | all of C, plus a record amendment | schedule builder + audit |

On #787: **only D creates the obligation.** `Yielded::ids` sorts, and
`option_d_yielded_set_is_canonical_not_iteration_order` proves the sort is
load-bearing by feeding two permutations and requiring one answer — the same
property `tier_h_projection_differential.rs` tests, now pushed into the record
itself rather than the projection.

---

## 7. Recommendation

**Adopt A. Do not build B, C or D.**

Stated plainly, and not because A is cheapest:

1. **The problem A is accused of having is one call site**, and that call site
   asks by name from the signed inputs. It has no use for a query's selection
   powers (§1).
2. **A is already the type-level guarantee the brief wants**, via the
   `Observation`/`System` split in `sched.rs`. "Status quo" understates it (§2).
3. **B is strictly worse than A.** It moves the compile-time guarantee from the
   lookup to the dereference, and absence — the case dishonesty wants — falls
   through the gap (§4). It also does not close enumeration, which was its
   entire competitive claim over A.
4. **C is A.** A logging `get` by named id, capped, is `StateView::neighbor`.
   C's only difference is a `PersistId`→`Entity` translation table that exists
   solely because the state lives in a `World`. Adopt C only as a consequence of
   adopting native, never on its own merits.
5. **D buys one thing** — closing an enumeration gap that only exists if
   something else already opened it — and pays for it with the cap, the replay
   bound, the meaning of a signed window, and a new #787 obligation. §3 shows
   the architecture already has a cheaper answer to the same question:
   enumerate outside the step, where the answer can only propose.

**The tradeoff A accepts, stated:** #796's ergonomic win on `speed_cap` and
`acceleration_cap` stays out of reach in the form #796 wrote it. That win is
real. But it is a *projection* win, and `sched.rs` already says projection is
the half that is allowed — so the honest next step is #796's own recommendation:
give `invariants.rs` the `projected_system!` treatment
(`crates/orrery_games/src/regolith/mod.rs:14-59`) and take the part of the win
that costs nothing.

**If native is ever adopted anyway**, adopt **E with C**, not B or D. E is the
only mechanism that restores a guarantee that does not depend on what the game
author chose to put in a signature, and C is what the call site becomes.

---

## 8. What I could not test, and why

- **No option is wired into the shipped step.** `tier_h_world_of_one` and
  `tier_h_adjudication_substrate` are green (6/6), but that proves *nothing else
  broke* — it does not prove "option X preserves world-of-one replay". Wiring
  B/C/D through `Regolith::step`, `Executor`, `EcsBackend` and `ReplayHarness`
  is a port, not a spike, and I judged the §4 finding decisive enough that
  building three ports to confirm it would spend the lane on a foregone
  conclusion. The replay-column entries in §6 are reasoned from
  `replay.rs:275-278` and `replay.rs:325-327`, which are quoted, not from a run.
- **D's log growth is computed, not measured on a wire.** The per-entity-tick
  id counts are exact (they are the scenario populations, and the executor
  serves neighbours from a tick-start slot holding every other live entity). The
  resulting *byte* growth depends on whether a yielded id is recorded as a bare
  `PersistId` or as a full `NeighborFrame` with a payload — a design choice D
  would have to make, and I did not pick one for it.
- **E was not run against a real Regolith schedule**, only against synthetic
  systems with the three access shapes that matter. Doing it for real requires
  the native port that #796 recommends against.
- **`core-gates.sh` fails with exactly `orrery_games has Bevy in its dependency
  graph`** — the expected failure inherited from #796's branch, and the only
  line it prints. The gate is not edited. `cargo fmt --check` clean;
  `cargo clippy -p orrery_games --all-targets` clean.

---

## 9. Amendments named as owed, not made

Unchanged from #796 — this branch adds no new violation, because it adds no new
dependency:

- **D42 (a)** and **D43 (e)(1)** — the Bevy-free ban on `orrery_games`, violated
  by the inherited `bevy_ecs` dependency.
- **D43 (e)(5)** — not amended, and this spike's conclusion is that it should
  not be: §4 is an argument that its discipline is load-bearing in a way the
  query-shaped alternatives cannot reproduce.
- **The `core-gates.sh:220-222` comment fix** #796 identified, still owed and
  still independent of this decision.

Nothing in this branch is proposed for merge.
