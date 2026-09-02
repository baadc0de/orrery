# Spike — A macro DSL for gameplay code

**Propose-only. Do not merge.** Sanctioned under
[#793](https://github.com/baadc0de/orrery/issues/793) as **evaluation, not
adoption**. Follows [#820](https://github.com/baadc0de/orrery/pull/820), whose
two measured divergences this design is priced against. Regolith is not
migrated, `Ruleset::step`'s signature is untouched, `orrery_core` is not
amended, `scripts/core-gates.sh` is unchanged, no ADR is amended, and
`bevy_ecs` is neither forked nor vendored.

A note on citations. The brief for this lane cited `executor.rs:394` for the
neighbour framing; on `main` at this branch's merge-base that code is at
`executor.rs:485-518`. Every line reference below was re-read before it was
repeated.

Branch: `spike/gameplay-macro-dsl`. Code: `spikes/orrery_rules_dsl/`.
Evidence: `spikes/orrery_rules_dsl/tests/recorded_reads.rs` (11 tests) and
`spikes/orrery_rules_dsl/tests/regolith_port.rs` (2 tests), all passing.

---

## 0. The verdict, first

**A macro DSL carries this. It is not, however, the answer to #820, and saying
otherwise would be the dishonest version of this report.**

Four findings, in the order that matters:

| | Finding | Evidence |
|---|---|---|
| **1. #820's two divergences are already prevented in this tree, by `StateView`.** | `StateView::neighbor` refuses the reader's own id by identity (`ruleset.rs:176`) and hides an observation past the cap with the same `checked_sub` the log uses (`ruleset.rs:192` / `executor.rs:485-518`). Both of #820's failures were properties of a **second** read path that was not told who was reading or when. A DSL that routes reads through `StateView` inherits the refusals; it does not invent them. | §1.1, §4 |
| **2. What is actually unenforced is not the read — it is everything declared *about* the read.** | The cap is a number restated three times (`visibility.rs:47`, `mod.rs:172`, `mod.rs:608`). The read set is an enum a game invented for itself. The declared schedule is written out twice and held together by a test (`mod.rs:864-874`, `mod.rs:1923`). The confinement is a `grep` the gate's own comment calls "a review trigger rather than a safety property" (`core-gates.sh:382-384`). | §1.2 |
| **3. The macro's real yield is that those four become structural — and the code gets *shorter*.** | Regolith's read plumbing is 42 lines (`visibility.rs:24-61`, `169-172`); the declared form is a 20-line list, 16 without its doc comments. The cap becomes `<ClaimFrames as RecordedReads>::MAX_NEIGHBOR_READS`. The manifest and the runnable table come from one source. Three runtime tests become compile errors with written-out messages. | §3, §5.1 |
| **4. The remaining hole is not one a macro can close, and a language would not close it either.** | The macro is opt-in: `Ruleset::step` still takes `&mut StateView` and `StateView::neighbor` is still `pub`, so a game that declines the form reads however it likes. Closing that is `orrery_core` narrowing its surface — owed work, named in §8, and cheaper than either a DSL or a language. | §6 |

**Recommendation: adopt B (the macro DSL), and do not build C.** But adopt it
for finding 3's reasons, not for #820's. And note what falls out of finding 4:
the single highest-value change in this whole report is not a macro at all — it
is making `StateView::new` unreachable from a game (§8.1), which is a
twenty-line diff.

A caveat that emerged only from building the thing, recorded here rather than
buried: the first version of the applier received the neighbour *states* and
not their identifiers, and Regolith's `Outcome::LockVisibility { locker, .. }`
promptly became inexpressible — an emitted event has to *name* a neighbour, and
no amount of its state supplies its `PersistId`. The signature was widened to
carry the resolved targets (§2.3). It is exactly the class of thing a design
document alone would have missed.

---

## 1. What the status quo actually enforces — read the lines first

### 1.1 The read itself is fine

`StateView::neighbor` (`crates/orrery_core/src/ruleset.rs:176-199`) does three
things in one expression:

```rust
pub fn neighbor(&mut self, id: PersistId) -> Option<&S> {
    let readable = id != self.entity;                       // reader identity
    let fresh = readable && self.observation_ticks.is_none_or(|(observed, tick, cap)| {
        observed.get(&id).is_some_and(|observed_tick| {
            tick.0.checked_sub(observed_tick.0).is_some_and(|age| age <= cap)
        })                                                  // staleness, at the read
    });
    let found = fresh.then(|| self.neighbors.get(&id)).flatten();
    if !self.reads.contains(&id) { self.reads.push(id); }   // recorded either way
    found
}
```

`canonical_step` (`executor.rs:485-518`) frames the same three decisions with
the same `checked_sub`, so the log and the live read agree by sharing the
arithmetic rather than by two implementations happening to match.

**Both of #820's divergences are refused here.** The own-id read is refused by
`id != self.entity`; the stale read is hidden by the `checked_sub`. #820's
`OrderedQuery::get` diverged because it had neither the reader nor the tick —
which is what its §3 concluded and what its `ReadWindow` was invented to
supply. This is worth being blunt about: **a macro DSL prevents #820's
divergences exactly as much as today's code does, and by the same mechanism.**

### 1.2 What is not fine

Four things, all verified against the cited lines.

**(a) The cap is restated three times.** `MAX_NEIGHBOR_READS` is
`NEIGHBOR_READ_SLOTS.len()` (`visibility.rs:47`), re-exported as
`pub const MAX_NEIGHBOR_READS: usize = visibility::MAX_NEIGHBOR_READS`
(`mod.rs:172`), and returned from `Ruleset::max_neighbor_reads`
(`mod.rs:608-610`). Regolith keeps the chain honest; nothing in `orrery_core`
requires it. A second `Observation` added to `OBSERVE` reads whatever it likes
and the cap does not move — the violation surfaces at
`replay.rs:275-278`, as a *malformed window*, on an authority that did nothing
but add a rule.

**(b) The read set is a convention a game invented.** `NeighborReadSlot`,
`NEIGHBOR_READ_SLOTS` and `impl NeighborReadSlot::target`
(`visibility.rs:24-61`) are excellent discipline and entirely local to
Regolith. `orrery_core::Observation` hands a body `&mut StateView` and asks for
nothing.

**(c) The confinement is a text scan, and it greps for the safe path.**
`core-gates.sh:389` is

```bash
neighbor_hits=$(grep -nE '\bview\.neighbor\s*\(' "${RULES_SOURCES[@]}" ...)
```

with the enclosing function found by an `awk` for the nearest `fn` above the
hit, checked against a one-entry `AUDITED_NEIGHBOR_PREDICATES`
(`core-gates.sh:394`). Two consequences follow from reading the regex. A read
through any binding not called `view` — `let v = &mut *view; v.neighbor(id)` —
is not matched — checked against the gate's own regex, which reports no hit
for `let v = &mut *view; v.neighbor(id)`. And a read that is not
`StateView::neighbor` at all is not
matched **by construction**: the gate detects the *sanctioned* path and is
blind to every unsanctioned one, which is precisely the shape #820 built. The
gate's own comment says this, in the same words the finding needs:

> What this gate buys is the tripwire: no code starts reading neighbours
> without a human seeing it. […] it is honest about being a review trigger
> rather than a safety property.

**(d) The declared schedule is written twice.** `mod.rs:864-874`:

> It is written out rather than derived because `CanonicalSchedule` is a
> `const` of `&'static` slices and the runnable table holds function pointers;
> deriving one from the other in a `const` context is not expressible today.
> The two are held together by
> `schedule_tests::the_declared_schedule_matches_the_table_that_runs`.

and, two paragraphs later, on ordering edges:

> D43 clause (c)(1) asks for an explicit edge on *every* pair with conflicting
> data access, derived mechanically; that derivation needs per-system
> data-access declarations, which this design does not yet carry.

Both sentences are accurate, and both describe a problem a macro is the natural
shape for: a macro does not derive one table from the other, it **emits both
from one source**.

---

## 2. The design

Two macros. `spikes/orrery_rules_dsl/src/lib.rs` and `.../src/schedule.rs`.

### 2.1 What it completes

`projected_system!` (`crates/orrery_games/src/regolith/mod.rs:6-31`) took the
projection half of an ECS query and left the selection. `section_invariant!`
(`orrery_core/src/invariants.rs:147`) lifted a per-section check into a
whole-state `Invariant`, refusing a check written for another section at the
expansion site by *annotating a local `fn` pointer rather than inferring it*.

That annotation trick is the whole enforcement mechanism reused here, and both
precedents stop one step short of the same place: an `Observation` still
receives a `&mut StateView`, so the recorded read is confined to a **signature**
but not to a **shape**.

### 2.2 `recorded_reads!`

An observation is split into two author-written pure functions, neither of
which is handed a `StateView`:

```text
resolve : (reader, &own, &inputs)                            -> Targets
apply   : (reader, &own, &Targets, &Frames, &inputs, &mut L) -> ()
```

and the macro generates the only expression between them:

```rust
let reader = StateView::entity(view);
let targets = resolve(reader, view.own(), inputs);
let frames = Frames {
    cover_locker: targets.cover_locker.and_then(|id| view.neighbor(id).cloned()),
    cover_rock:   targets.cover_rock.and_then(|id| view.neighbor(id).cloned()),
    collision:    targets.collision.and_then(|id| view.neighbor(id).cloned()),
};
apply(reader, view.own(), &targets, &frames, inputs, locals);
```

The declaration:

```rust
recorded_reads! {
    /// Regolith's audited claims read, in the declared form.
    pub REGOLITH_CLAIM_READS {
        rules:   Regolith,
        locals:  RegolithLocals,
        system:  "verify-claims",
        targets: ClaimTargets,
        frames:  ClaimFrames,
        slots:   [
            /// The craft whose lock a cover claim challenges.
            cover_locker,
            /// The rock a cover claim names as occluder.
            cover_rock,
            /// The counterparty a collision claim names.
            collision,
        ],
        resolve: claim_targets,
        apply:   verify_claims,
    }
}
```

| Guarantee | Mechanism | Test |
|---|---|---|
| Every neighbour read is on the recorded path | Neither author function can name a `StateView`; the annotated `fn` pointers refuse a signature that does | §5.2 case (a) |
| Reader identity stamps the read | `reader` is `view.entity()`, supplied by the expansion; `StateView::neighbor` refuses it | `the_readers_own_id_is_refused_recorded_and_framed_absent` |
| Staleness applied at the read | The read *is* `StateView::neighbor` — one implementation of `checked_sub`, not two | `an_observation_past_the_staleness_cap_is_hidden_and_framed_absent`, `..._exactly_at_the_staleness_cap_is_delivered` |
| No way to name a neighbour except through it | The author vocabulary is `PersistId`, `CoreState`, `CoreInput`, `Locals`. Nothing reaches a store | §5.2 case (e) |
| Ordering established, not inherited | Slot order is declaration order, fixed in the generated struct literal; a resolver returns a fixed-shape record and cannot reorder, grow or shrink the set | `slot_declaration_order_is_recorded_first_read_order` |
| The cap is derived | `<Frames as RecordedReads>::MAX_NEIGHBOR_READS` is the slot count; the ruleset reads it off the type | `the_declared_cap_is_the_slot_count_...`, `the_declared_cap_is_regoliths_published_cap` |
| Engine handles unrepresentable | Closed under the signatures the macro admits — the same closure argument #815 §2.3 makes, holding trivially because the generated code is the only thing that touches a view | — |

### 2.3 The correction the build forced

The first applier signature was `(reader, &own, &Frames, &inputs, &mut L)`.
Porting `verify_visibility` broke immediately: `Outcome::LockVisibility { locker, target, occluded }` (`order.rs:587`) carries the **locking craft's `PersistId`**,
and a `Craft` does not know its own id. The applier now receives `&Targets` as
well. Targets are plain `Option<PersistId>` and carry no capability, so this
widens what an applier may *say* without widening what it may *read* — and it
is the difference between a design that compiles against real Regolith code and
one that does not.

### 2.4 `canonical_schedule!`

Emits `Schedule<R, L>` (the runnable table) and `CanonicalSchedule` (D43 clause
(g)'s manifest) from one list, and checks their agreement in `const` context:

```rust
canonical_schedule! {
    rules:    Macro,
    locals:   Locals,
    runnable: pub MACRO_SCHEDULE,
    declared: pub MACRO_CANONICAL,
    observe:  "observe" => [ "verify-claims" => CLAIM_READS ],
    stages:   [ "fold" => [ "accrue" => accrue ] ],
    edges:    [ "verify-claims" -> "accrue" ],
}
```

Three things that are tests today become compile errors: a duplicated system
name, an ordering edge disagreeing with the runnable order (or naming a system
that does not exist), and an observation declared under a name other than the
one it carries. The stage/system agreement is not checked at all, because one
list produces both tables and there is nothing left to disagree.

The `edges:` list is still hand-written. Deriving it mechanically needs
per-system read/write declarations, and doing it in `macro_rules!` means an
O(n²) TT-munch whose expansion is the cost — see §7.

---

## 3. Before and after, on real Regolith code

`spikes/orrery_rules_dsl/tests/regolith_port.rs` compiles the "after" against
the real `Regolith`, the real `RegolithState`, the real `Order` and the real
`Outcome`. It is not a mock-up and it is not wired in.

### 3.1 What disappears

`crates/orrery_games/src/regolith/visibility.rs:24-61` — the slot enum, its
doc, the array, the derived cap and the `target` impl — **38 lines**, plus the
read expression at `169-172` — **4 lines**. 42 lines total, all of them lines a
reviewer has to read carefully.

```rust
// before — visibility.rs:24-61 (abridged) and 169-172
#[derive(Clone, Copy)]
enum NeighborReadSlot { CoverLocker, CoverRock, Collision }

const NEIGHBOR_READ_SLOTS: [NeighborReadSlot; 3] = [
    NeighborReadSlot::CoverLocker, NeighborReadSlot::CoverRock, NeighborReadSlot::Collision,
];
pub(crate) const MAX_NEIGHBOR_READS: usize = NEIGHBOR_READ_SLOTS.len();

impl NeighborReadSlot {
    fn target(self, cover: Option<(PersistId, PersistId)>, collision: Option<PersistId>)
        -> Option<PersistId>
    {
        match self {
            Self::CoverLocker => cover.map(|(locker, _)| locker),
            Self::CoverRock   => cover.map(|(_, rock)| rock),
            Self::Collision   => collision,
        }
    }
}
// ...
let [locker, rock, collision] = NEIGHBOR_READ_SLOTS.map(|slot| {
    slot.target(cover, collision_id)
        .and_then(|id| view.neighbor(id).cloned())
});
```

replaced by the 20-line declaration in §2.2 — 16 lines without its four doc
comments, which are new documentation rather than replaced code. **Net −22
lines, and the arithmetic that remains is a list.**

### 3.2 What does not change at all

`verify_visibility` (`visibility.rs:195-231`) and `verify_collision`
(`visibility.rs:233-300`) already take borrowed states rather than a view.
`verify_collision`'s signature is *literally* an applier's:

```rust
fn verify_collision(me: PersistId, own_state: &RegolithState, other_id: PersistId,
                    other_state: &RegolithState, overflowed: &mut bool)
    -> Option<CollisionResolution>
```

**The migration is not a rewrite of Regolith's rules.** It is the deletion of
read plumbing. That is the strongest ergonomic evidence in this report, and it
is also the reason the port could be compiled at all.

### 3.3 One rule gets *shorter* for a reason worth naming

`verify_visibility` opens with three identity guards (`visibility.rs:209-211`):

```rust
if locker_id == rock_id || locker_id == target_id || rock_id == target_id { return None; }
```

Two of the three exist because the hand-written form carries identifiers
alongside states and has to re-check what `StateView::neighbor` already
refused. In the declared form the reader's own row arrives as `None`, so
`locker_id == target_id` and `rock_id == target_id` have nothing to compare —
they are *unrepresentable outcomes*, not omitted checks. The third,
`locker_id == rock_id`, is a genuine rule (one entity cannot be both the locker
and the occluder) and survives as a check on the targets, in the resolver,
where it belongs. `the_ported_predicate_still_refuses_a_locker_that_is_also_the_occluder`
exercises both branches.

### 3.4 `invariants.rs` and `Regolith::step` — no change proposed

The brief named both as before/after candidates. Honestly: neither needs one.
`section_invariant!` already did that work (#791/#802), and `Regolith::step` is
already `orrery_core::run_schedule(self, view, inputs, rng)` — one line
(`mod.rs:630`). Proposing a rewrite of either would be inventing a problem.

---

## 4. The three-way comparison, against #820's measured divergences

Priced against what #820 actually measured, not against imagined failures.

|  | #820 (a) — own-id read | #820 (b) — stale read | A new read path *at all* |
|---|---|---|---|
| **A. Status quo + gates** | **Prevented by construction**, at `ruleset.rs:176`, for reads through `StateView` | **Prevented by construction**, at `ruleset.rs:192` + `executor.rs:485-518`, sharing one `checked_sub` | **Open, and the detector is blind to it.** `core-gates.sh:389` greps for `view.neighbor(`; a path that is not that expression is not scanned, and #820's `OrderedQuery::get` is exactly such a path |
| **B. Macro DSL** | **Prevented by construction**, *inherited* — the read is `StateView::neighbor` | **Prevented by construction**, *inherited*, same call | **Not reachable from a rule.** An author holds `PersistId`, `CoreState`, `CoreInput`, `Locals` and no `view` binding; §5.2(e) is the compile error. Still **open at the crate level**: a game that can name a `World` can build one, which is #815's engine-handle bound, not this macro's job |
| **C. Standalone language** | Prevented by construction | Prevented by construction | Prevented by construction — the vocabulary is closed by the language, not by a macro's opt-in discipline |

Read down the first two columns: **A, B and C are identical on both of #820's
divergences.** That is the finding, and arguing otherwise would be arguing
toward the answer the brief hoped for. The three options differ only in the
third column, and B closes the part of it that a rules author can reach for the
price of a `macro_rules!`, while C closes the rest for the price of a research
team.

### 4.1 What each merely checks

* **A** checks the cap and staleness at `replay.rs:259-278`, after the fact, as
  *window malformedness* — which convicts the log rather than the rule. It
  checks confinement with a `grep`. It checks the schedule agreement with
  `the_declared_schedule_matches_the_table_that_runs` (`mod.rs:1923`).
* **B** checks nothing that A checks structurally; it moves cap, confinement,
  slot ordering and schedule agreement from "checked" to "constructed". It
  leaves the *replay-side* checks exactly where they are, which is correct —
  an adjudicator must not trust the authority's build.
* **C** would additionally close the crate level, and would own a compiler, a
  debugger story, a language server, an error-message budget and a hiring
  problem. Epic bought transactional rollback with theirs. **Orrery's rollback
  is forward correction (#812), not speculative execution**, so the single
  largest thing Verse purchased is a thing this system has already decided it
  does not need.

---

## 5. What it costs

### 5.1 Compile time — measured, not guessed

A generated probe declaring **32** observations in the macro form and **32**
hand-written equivalents in the same crate, timed as incremental rebuilds of
the test target (median of three, after one warm-up):

| Form | Median rebuild | Source lines |
|---|---|---|
| `recorded_reads!` × 32 | **0.28 s** | 723 |
| hand-written × 32 | **0.27 s** | 1 203 |

Scaling to **256** declarations: 0.86 s over 4 755 lines. Roughly linear in
declarations with the fixed crate cost dominating at these sizes — **no
super-linear expansion blow-up**, which is the thing `macro_rules!` is actually
bad at and the thing worth measuring. The declared form also produces 40% fewer
source lines for the same declarations.

Whole-crate figures for context: `cargo build -p orrery_rules_dsl` from cold
(dependencies included) is ~11 s; `cargo test -p orrery_rules_dsl` compiles and
runs 13 tests in ~23 s cold, ~1 s warm.

### 5.2 Error quality — the actual diagnostics

Six deliberate mistakes, compiled, output quoted verbatim. A seventh — a
mistyped macro key — is in §5.3, where it belongs to the completion story.

**(a) An applier that asks for a `StateView`.**

```
error[E0308]: mismatched types
   --> tests/zz_broken.rs:152:18
135 | / recorded_reads! {
...   |
152 | |         apply:   fold_claims,
    | |                  ^^^^^^^^^^^ incorrect number of function parameters
154 | | }
    | |_- expected due to this
    = note: expected fn pointer `for<'a, 'b, 'c, 'd, 'e, 'f> fn(PersistId, &'a Body, &'b ClaimTargets, &'c ClaimFrames, &'d OrderedInputs<'e, Order>, &'f mut Locals)`
                  found fn item `fn(PersistId, &Body, &ClaimTargets, ..., ..., ..., ...) {fold_claims}`
    = note: the full name for the type has been written to '.../zz_broken-<hash>.long-type-<hash>.txt'
```

Good: the caret is on the `apply:` key and the *expected* signature is printed
in full. Bad, and worse than expected: at six parameters rustc's type-length
threshold trips and the **found** signature is elided to `...`, so the note no
longer says what the author actually wrote. The mistake is recoverable — the
expected side plus the parameter count identifies it, and the full type is in a
file — but this is the single worst diagnostic the design produces, and it gets
worse as the applier's signature grows. A `proc-macro` reporting the mismatch
itself, with a span on the offending parameter, would not have this failure
mode. Recorded in §7 as part of the case for one.

**(b) A resolver returning the wrong shape.**

```
error[E0308]: mismatched types
151 | |         resolve: claim_targets,
    | |                  ^^^^^^^^^^^^^ expected fn pointer, found fn item
    = note: expected fn pointer `for<...> fn(PersistId, &Body, &OrderedInputs<_>) -> ClaimTargets`
                  found fn item `for<...> fn(PersistId, &Body, &OrderedInputs<_>) -> (Option<PersistId>, Option<PersistId>, Option<PersistId>) {claim_targets}`
```

**(c) An ordering edge declared backwards.**

```
error[E0080]: evaluation panicked: a declared ordering edge disagrees with the
table that runs, or names a system the table does not contain
   --> tests/zz_broken.rs:200:1
200 | / canonical_schedule! {
```

**(d) Two systems sharing a name.**

```
error[E0080]: evaluation panicked: two canonical systems share one name: an
ordering edge addressing it is ambiguous and the schedule digest describes
something that does not exist
```

**(e) A system reaching for a neighbour.**

```
error[E0425]: cannot find value `view` in this scope
195 |     if let Some(sneaky) = view.neighbor(PersistId::new(999)) {
    |                           ^^^^ not found in this scope
```

The best of the six: it points at the author's own line, and it is the error
that replaces `core-gates.sh` clause 5's grep.

**(f) An observation declared under a name it does not carry.**

```
error[E0080]: evaluation panicked: an observation is declared under a different
name than the one it carries
```

**Verdict on error quality: better than expected, worse than a `proc-macro`
could do.** (c), (d) and (f) are as good as errors get — the message *is* the
explanation, and it was written by whoever wrote the rule. (b) is an ordinary
`E0308` with the caret one indirection away from the mistake — the standard
`macro_rules!` tax, exactly what `section_invariant!` already pays today. (a)
is the one real complaint: rustc elides the signature the author wrote. (e) is
free.

### 5.3 IDE and `rust-analyzer`

Not measured under instrumentation; what follows is what the shapes imply and
should be treated as such.

* **Go-to-definition on `resolve`/`apply` works**, because they are ordinary
  `fn` items named at the call site. This is the single biggest thing the
  split-function design buys over a macro that swallows the body: the code an
  author writes and debugs is never inside a macro.
* **Completion inside `Frames`/`Targets` works**, because both are real
  `struct`s with real fields, generated once.
* **Completion of the macro's own key/value grammar does not exist**, as for
  every `macro_rules!`. A mistyped key is better handled than expected — the
  key/value grammar makes the arm unambiguous, so rustc names the key it was
  trying to match and points at the macro's own definition:

  ```
  error: no rules expected `resolvee`
  151 |         resolvee: claim_targets,
      |         ^^^^^^^^ no rules expected this token in macro call
  note: while trying to match `resolve`
     --> src/lib.rs:175:13
  ```

  A positional macro would have produced a token-soup error here instead. The
  named-key grammar is worth the extra verbosity for this reason alone.
* `Frames` and `Targets` are macro-generated types, so hovering them shows the
  generated form. `section_invariant!` already puts games in this position.

### 5.4 Debuggability of generated code

The generated body is eleven lines and contains no logic: two annotated `let`
bindings, a `view.entity()`, one call, one struct literal, one call. Everything
an author can get wrong is in a function they wrote, with a name, that
`orrery_core::run_system` can already drive standalone (`sched.rs:287`). A
breakpoint in a generated `fn run` is possible and rarely wanted.

The honest cost is elsewhere: **the read moves out of the game crate's source
text.** `core-gates.sh` clause 5's grep would find `view.neighbor(` at the
macro's definition site and attribute it, via its "nearest `fn` above"
heuristic, to a generated `fn run`. In the shipped design the macro lives in
`orrery_core`, which clause 5 exempts by name ("`orrery_core` defines and tests
`StateView::neighbor`, so it must be able to name it",
`core-gates.sh:57-58`) — so the gate would report **zero** audited predicates
in `orrery_games` and the review tripwire would move from "which functions read"
to "which slots are declared". That is a better tripwire, but it is a different
one, and adopting the macro means editing clause 5 rather than leaving it
standing.

---

## 6. What remains unenforced

Stated to #820's standard, because that honesty is why its findings are
trusted.

1. **The form is opt-in.** `Ruleset::step` takes `&mut StateView`,
   `StateView::neighbor` is `pub`, and a game can implement `Ruleset` directly
   and read as it likes. The macro raises the floor for code that uses it and
   does nothing about code that does not.
2. **`StateView::new` is `pub` and builds a view with `observation_ticks:
   None`** (`ruleset.rs:112-122`) — staleness entirely off. It is the same
   shape #820 flagged as `ReadWindow::open()` and refused to ship. Today its
   only callers are tests (`orrery_conformance/tests/quantize_pin.rs:122`,
   `ruleset.rs:427`, `ruleset.rs:446`, `sched.rs:429`), but nothing prevents a
   fifth.
3. **The macro cannot stop a crate from acquiring a store.** #820's arrangement
   put a `World` behind a `Mutex` inside the rules object. No `macro_rules!`
   reaches that; it is the engine-handle bound (#815, #798) and stays there.
4. **Nothing forces a ruleset to use the derived cap.** `fn
   max_neighbor_reads(&self) -> usize { 3 }` still compiles. Making it
   structural needs the cap to come off the *schedule* rather than off a
   method — see §8.3.
5. **Ordering edges are still hand-written**, so D43 clause (c)(1)'s "every pair
   with conflicting data access" is still a hand-picked subset. What changed is
   that a wrong edge no longer compiles.
6. **Dependent reads inside one observation are inexpressible.** A read whose
   target depends on an earlier read of the same observation has nowhere to go,
   because `resolve` runs before any read happens. It is expressible across two
   declared observations, whose caps sum — arguably the honest form, since a
   chained read is a distinct recorded fact, but it is a restriction and
   Regolith is simply not exercising it today.
7. **The port is a port, not a migration.** `regolith_port.rs` proves the
   declaration is expressible against Regolith's real types and that the
   predicate needs no change. It is not wired into `REGOLITH_SCHEDULE`, and
   `PortLocals` stands in for `visibility::VerifiedClaims`, which is
   `pub(crate)`. The collision predicate is not transcribed at all — §3.2 is an
   argument from its signature, not a compiled fact.
8. **One entity, one observation, three slots, three ticks, one component
   type.** Nothing exercises `step_tick`, multi-entity ordering, materialization
   or `orrery_sim_host`. The replay leg is a single-entity chain with no
   siblings.
9. **`canonical_schedule!` is not exercised against a real digest.** It emits a
   `CanonicalSchedule`; nothing here feeds one to `orrery_compose`'s validator
   or recomputes D43 clause (g)'s digest over it.

---

## 7. Why not C, priced honestly

A standalone language would close §6.1 and §6.3, which a macro cannot. Against
that:

* **The rollback argument does not transfer.** Verse's transactional semantics
  buy speculative execution and rollback. Orrery's correction path is *forward*
  (#812) — an authority publishes a correction and peers converge; nothing is
  speculatively executed and un-executed. The single most expensive thing Epic
  built is the thing this system has already decided against.
* **Two of Orrery's determinism rules are about the *host platform*, not the
  language.** VC-6 is "`libm`, not std transcendentals, compared within
  tolerance bands" and VC-4 is "no unordered iteration". A new language would
  have to reimplement both against the same `libm` and the same ordered
  containers to reach parity with a `grep` and a `BTreeMap`.
* **The adjudicator has to link the rules.** `orrery_core` is a non-dev
  dependency of eleven crates (`core-gates.sh:222-226`), including `persistd`.
  A language means an interpreter or a codegen backend in the adjudication
  path, and "the adjudicator ran a different build of the rules" is the exact
  failure the verifiable core exists to detect.
* **The measured problem does not need it.** §1.2's four items are all
  *declaration* problems. A macro is the tool for turning a restated fact into
  a declared one.

A middle option not in the brief, recorded because it is the natural next step
if B is adopted: **a procedural macro**. It buys three things `macro_rules!`
cannot — key/value completion and precise spans (fixing §5.2(a)/(b) and the
mistyped-key diagnostic), mechanical derivation of ordering edges from
per-system access declarations without an O(n²) TT-munch (§6.5), and the
ability to emit `SystemId`s from the same tokens without the caller repeating
a literal. It costs a `syn`/`quote` dependency in the build graph of a rules
crate and a slower cold build. Not recommended now; recommended before edges
are derived.

---

## 8. Owed, not done

Named as owed work, in the shape #820's §5 uses. **Nothing here is done in this
branch.**

**8.1 Make `StateView::new` unreachable from a game.** §6.2. The cheapest and
highest-value change in this report and it has nothing to do with a DSL:
`pub(crate)` plus a `#[doc(hidden)]` test constructor, or a `StateView::sealed`
taking a token only `orrery_core` can mint. Four callers, all tests. This is
what closes the `ReadWindow::open()` shape *in the tree that exists* rather than
in the one #820 proposed.

**8.2 Decide where the macro lives, and amend clause 5 with it.** §5.4. In
`orrery_core` it is exempt and `AUDITED_NEIGHBOR_PREDICATES` empties for
`orrery_games`; that is an edit to `scripts/core-gates.sh` and a paragraph of
review reasoning, not a configuration change.

**8.3 Derive the cap from the schedule, not from a method.** §6.4.
`Scheduled::schedule()` knows every declared observation; summing their
`MAX_NEIGHBOR_READS` would make `Ruleset::max_neighbor_reads` a provided method
a game cannot get wrong. It needs a trait-level seam this spike did not design
and it touches `orrery_core`.

**8.4 D43 (e)(5) and ADR-0043 (b).** #820 named this and it survives here in a
weaker form: clause (e)(5) reads as though `StateView` is the only recorded read
path. This design keeps that true — the macro *is* `StateView` — so nothing is
owed unless §8.3 moves the cap, which would put a second statement of the read
bound in the schedule. **No ADR is amended here.**

**8.5 The `Ruleset::step` signature.** #820 concluded a real bevy-native path
must change it. This design does **not**: `run_schedule` already has exactly
`step`'s arguments and return type (`sched.rs:246`), and every macro here sits
above that. If §8.3 is taken, the change is additive (a provided method), not a
signature change. Recorded so the two spikes' conclusions are not conflated.

---

## 9. Evidence index

| Claim | Where |
|---|---|
| The two forms are byte-identical through the whole evidence path | `recorded_reads.rs::the_macro_form_and_the_hand_written_observation_produce_identical_windows` |
| A macro-authored window replays clean on a hand-written adjudicator | `..._replays_clean_on_the_hand_written_one` — `Executor` → `InputLogProducer` → signed `LogFrame`s → `ReplayHarness::replay`, three ticks, hashes compared per tick |
| Not vacuous | `deleting_a_slot_changes_the_window` (169 → 136) |
| #820 (a), own-id | `the_readers_own_id_is_refused_recorded_and_framed_absent` — read recorded, frame `present: false`, 118 not 218 |
| #820 (b), stale | `an_observation_past_the_staleness_cap_is_hidden_and_framed_absent` (112) and `..._exactly_at_the_staleness_cap_is_delivered` (123) — the bound is a bound, not a switch |
| Cap derived | `the_declared_cap_is_the_slot_count_...`, `a_window_never_carries_more_frames_than_the_declared_cap` |
| Slot order is read order | `slot_declaration_order_is_recorded_first_read_order` — inputs arrive collision-first, reads come back in slot order |
| An unnamed slot reads nothing | `a_slot_resolving_to_none_records_no_read` |
| Both schedule tables agree | `the_declared_schedule_is_the_table_that_runs` |
| The declaration compiles against real Regolith types | `regolith_port.rs`, both tests |
| Regolith's published cap equals the declared slot count | `the_declared_cap_is_regoliths_published_cap` |
| The ported predicate keeps the guard that is a rule and loses the two that were re-checks | `the_ported_predicate_still_refuses_a_locker_that_is_also_the_occluder` |
