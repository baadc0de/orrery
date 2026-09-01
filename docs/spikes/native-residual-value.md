# Measurement — what would going `bevy_ecs`-native still add, after the seam widened?

**Read-and-report. No production code, no record amended, no implementation
opened.** This is the measurement promised when #802 landed, against the tree
as it stands (`cfbf607`). It answers the owner's question on
[#793](https://github.com/baadc0de/orrery/issues/793): now that the seam has
been widened, what would native still add?

Everything below was verified by reading `crates/orrery_games/src/regolith/`
at this commit. Earlier statements made against older trees — including ones
in #793's own comments — are re-derived here, not trusted.

---

## 0. The verdict, first

**The question decomposes into three classes, and the classes do not move
together.** Of what is still unpleasant in Regolith's code today:

1. **Contract surfaces** that live in `orrery_core` or in the adjudication
   wire format and that `bevy_ecs` in `orrery_games` cannot touch — including
   the `&R::CoreState` line #804 proved blocks narrow components (E0515).
2. **Declaration duplication** that Bevy's scheduler would derive mechanically
   — and that the #823 macro DSL would also derive, without the dependency.
3. **A small residue of sum-type matches** in game code, most of which the
   tree's own #802 mechanisms can retire without Bevy.

The dependency being *permitted* (#805) does not make the seam *passable*:
the load-bearing blocker is unchanged since #804 measured it, and it lives in
a crate the acceptance explicitly keeps Bevy-free.

---

## 1. What #802 actually bought (verified)

The claims in the brief check out against the tree.

**Sections are types.** `state.rs:490-568` declares `CraftSection`,
`RockSection`, `PickupSection`, `BloomDirectorSection`, each implementing
`orrery_core::Section` with an exact `project` (`state.rs:523-528`):

```rust
impl Section for CraftSection {
    type Root = RegolithState;
    type State = Craft;
    const SECTION: StateSection = SECTION_CRAFT;

    fn project(root: &Self::Root) -> Option<&Self::State> {
        match root {
            RegolithState::Craft(craft) => Some(craft),
            _ => None,
        }
    }
}
```

**Four of the six published invariants name their section in a signature.**
`invariants.rs:30-31` registers `speed_cap` twice against `CraftSection` and
`RockSection`; `acceleration_cap`, `fire_rate` and `score_rate` are written
against `&InvariantSample<'_, Craft>` and lifted by `section_invariant!`
(`orrery_core/src/invariants.rs:147-170`). `speed_cap`'s two discarded-zero
arms are gone in the strong sense — the trait that replaced them makes them
*inexpressible* (`invariants.rs:49-54`):

```rust
/// A section whose values move under a published velocity ceiling.
///
/// Implemented by craft and rock and by nothing else, which is the whole
/// content of the two arms `speed_cap` used to carry only to yield a zero the
/// comparison then discarded: pickups and bloom directors do not move, so they
/// have no ceiling to name, and now there is nowhere to write one.
trait SpeedLimited {
```

**`TickBackend::section_state<S>` is a provided method** (`executor.rs:706-711`),
with a doc comment stating the additive rationale: replacing `state` "would
put the adjudication path through a refactor to buy an ergonomic win that does
not need one" (`executor.rs:688-696`).

**And — the fact the original case for native was made against — Regolith's
rules are already systems over narrow component types.** Every rule in
`craft.rs` and `world.rs` is a named function over one component:

```rust
pub(crate) fn tick_cooldowns(craft: &mut Craft, _cx: &mut Cx<'_>) {
    craft.cooldown = craft.cooldown.saturating_sub(1);
    craft.cover_claim_cooldown = craft.cover_claim_cooldown.saturating_sub(1);
}
```

(`craft.rs:32-35`; `world.rs:36-38`, `:251-260`, `:332-334` are the same
shape over `Rock`, `Pickup`, `BloomDirector`.) The four-arm dispatch lives
once, in the `projected_system!` macro (`mod.rs:14-31`), whose own doc says
the intent:

> This is the projection half of an ECS query, and deliberately only that
> half. The runner never scans a population — it hands each system the one
> entity being stepped — so a rule cannot reach a neighbour it did not
> record, and the `match own { .. }` four-way dispatch that used to open
> `Ruleset::step` disappears into the table instead of being rewritten in
> every rule body.

#791's specific complaint — the four-arm match opening every invariant check
(`invariants.rs:42-52` of that tree) — no longer exists in any rule body.

---

## 2. What is still unpleasant

The inventory, with locations. Each item is classified in §3.

### 2.1 Sum-type matches surviving in game code

**`propagate_claim_overflow` — a live four-arm match** (`mod.rs:768-777`):

```rust
fn propagate_claim_overflow(state: &mut RegolithState, cx: &mut Cx<'_>) {
    if !cx.locals.claims.arithmetic_overflowed {
        return;
    }
    match state {
        RegolithState::Craft(craft) => craft.arithmetic_overflowed = true,
        RegolithState::Rock(rock) => rock.arithmetic_overflowed = true,
        RegolithState::Pickup(_) | RegolithState::BloomDirector(_) => {}
    }
}
```

This one is an idiom lag, not a ceiling: `arithmetic_overflowed` is a field
of both `Craft` and `Rock`, and two projected systems (or a small trait like
`SpeedLimited`) retire it today.

**`Game::trajectory`** (`mod.rs:1701-1708`) — the `Game` trait method over
the sum:

```rust
fn trajectory(state: &RegolithState) -> (QPos, QVel) {
    match state {
        RegolithState::Craft(craft) => (craft.pos, craft.vel),
        RegolithState::Rock(rock) => (rock.pos, rock.vel),
        RegolithState::Pickup(pickup) => (pickup.pos, QVel::default()),
        RegolithState::BloomDirector(_) => (QPos::default(), QVel::default()),
    }
}
```

Host-facing, not rule code. It could be per-section via `Section::project`
on the host side without any dependency change.

**`Body::from_state`** (`visibility.rs:75-103`) — a four-arm match building
a geometry record from the sum, with two `None` arms for sections that are
not bodies.

**`value_range`'s per-section halves** (`invariants.rs:211-277`) — four arms
of field-range checks over the sum, plus four pair arms of monotonicity
checks (`invariants.rs:278-334`). The per-section halves are mechanically
convertible to `section_invariant!` checks — the mechanism exists and is
already used four times in the same file. The pair arms narrow with
`InvariantSample::project`, which narrows `previous` independently
(`orrery_core/src/invariants.rs:93-98`).

**The codec dispatches** (`state.rs:606-615` for `Quantized`,
`state.rs:864-897` for `CoreCodec`, `state.rs:480-487` for
`Sectioned::section`) — the enum's own plumbing.

**`materialize`** (`mod.rs:632-684`) and **`deliver`** (`mod.rs:1580-1699`)
— but these are matches over `Outcome`, the *event* sum, not the state sum.
They are the executor's materialization and delivery contracts; no storage
design touches them.

### 2.2 The pair-shaped checks that survived #791 by design

`teleport` (`invariants.rs:120-166`) keeps the four-arm pair match, and the
file's own comment states why (`invariants.rs:33-37`):

> Teleport and value-range stay whole-state, and the reason is the same
> for both: they ask about a *pair* of samples spanning two sections. A
> craft that arrives where a rock was is the discriminant mismatch each of
> them reports, and no per-section signature can hold a question about the
> section changing.

The discriminant arm of `value_range` (`invariants.rs:327-331`) is the same
question — section *change* is itself the violation — and no component
signature can hold it either:

```rust
(previous, current)
    if core::mem::discriminant(previous) != core::mem::discriminant(current) =>
{
    return Err(InvariantViolation::new(InvariantKind::ValueRange, NAME))
}
```

### 2.3 Neighbour and own reads through the observation are sum-typed

`StateView<'a, S>` is generic over the whole sum (`orrery_core/src/ruleset.rs:102-110`),
so the one audited read expression yields the sum and every consumer
re-narrows it. `verify_visibility` (`visibility.rs:202`, `:212`):

```rust
let RegolithState::Craft(target) = view.own() else {
    return None;
};
```

```rust
let (Some(RegolithState::Craft(locker)), Some(RegolithState::Rock(rock))) = (locker, rock)
else {
    return None;
};
```

This is the construct components would most obviously fix — `Query<&Craft>`
*is* the narrowing — and it is also the one the contract forbids fixing:
the read is recorded (`StateView::neighbor` pushes before it knows whether
it found anything, `ruleset.rs:193-196`), and #820 proved two divergences of
exactly the query-shaped alternative (own-id reads that pass the log while
consuming own state; stale reads that convict an honest authority). The
owner's acceptance comment states the standing constraint: *"rules reach
neighbours through `StateView::neighbor` and nowhere else."*

Note the read plumbing has grown since the "one production neighbour read"
comment was written: there are now three declared slots
(`NEIGHBOR_READ_SLOTS`, `visibility.rs:41-45`) feeding one read expression
(`visibility.rs:169-172`). Still one expression; three reasons to call it.

### 2.4 The declaration burden

**The neighbour cap is stated three times** — `visibility.rs:47`,
`mod.rs:172`, `mod.rs:608` — and the read set is an enum Regolith invented
for itself (`visibility.rs:26-61`). `orrery_core::Observation` asks for
none of it. This is #823 §2's finding verbatim; it is unchanged.

**The schedule is written twice and held together by tests.** The runnable
table is `REGOLITH_SCHEDULE` (`mod.rs:806-831`); the declared, digested,
peer-shipped form is `REGOLITH_CANONICAL_SCHEDULE` with
`REGOLITH_SCHEDULE_STAGES` (`mod.rs:902-957`) and eighteen hand-written
`REGOLITH_SCHEDULE_EDGES` (`mod.rs:960-1039`); a pinned digest
(`mod.rs:859-862`) plus three tests (`mod.rs:1923`, `:1954`, `:1966`,
`:2027`) hold the halves together. The declared schedule's own doc comment
admits the gap (`mod.rs:886-892`):

> **On `ordering_edges`, honestly.** These are the load-bearing edges: the
> ones where swapping two systems changes canonical output. D43 clause (c)(1)
> asks for an explicit edge on *every* pair with conflicting data access,
> derived mechanically; that derivation needs per-system data-access
> declarations, which this design does not yet carry. What is here is a
> hand-written subset, checked against the runnable order rather than
> asserted.

**The macro family itself** (`projected_system!` and its four wrappers plus
`state_system!` and `observation!`, `mod.rs:14-85`) is ~70 lines of
game-local DSL whose function is to emulate what a query signature provides
for free.

### 2.5 The manual byte codecs

`CoreCodec for Craft` (`state.rs:617-707`) is hand-packed at literal offsets,
with the hazards that implies stated as guards:

```rust
fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
    if bytes.len() <= CRAFT_BASE_ENCODED_LEN
        || bytes[106] > 1
        || bytes[126] > 1
        || bytes[131] > 1
    {
        return Err(CodecError("regolith craft: wrong length"));
    }
```

(`state.rs:655-662`; `Rock`, `Pickup` and `BloomDirector` follow the same
shape at `:709-765`, `:767-815`, `:817-862`.) This is the most
error-prone surface in the crate and it is *canonical wire contract*
(D42 (b)(2), clause 1: additive-only; canonical encoding unchanged) —
no storage design touches it, Bevy or otherwise.

---

## 3. Which of those native would actually fix

### 3.1 The seam wall is not in `orrery_games`

The line #804 proved blocks narrow components is `TickBackend::state`
(`executor.rs:674`):

```rust
fn state(&self, entity: PersistId) -> Option<&R::CoreState>;
```

— a reference to the sum, which `orrery_sim_host/src/ecs.rs`'s module note
proves at the compiler (`ecs.rs:33-48`):

```text
error[E0515]: cannot return value referencing temporary value
    |     Some(&embed::<S>(held.0.clone()))
    |     ^^^^^^--------------------------^
    |     |     temporary value created here
    |     returns a value referencing data owned by the current function
```

and the note's own conclusion (`ecs.rs:50-52`): *"every decomposition of the
payload — per section, per field, or the `bevy_ecs`-native ruleset of #793 —
meets the same one line."* The other half is `canonical_step`
(`executor.rs:461-477`), which builds the view over the sum and hands it to
`Ruleset::step(&mut StateView<'_, RegolithState>, ...)` — the step itself is
sum-typed end to end.

**Both halves live in `orrery_core`, which stays Bevy-free by a separate,
live rule** — a non-dev dependency of eleven crates including `persistd` and
`witness` (the corrected count from #805), enforced by
`core-gates.sh:54`/`:259` where only `orrery_games` is exempt. Taking the
dependency in `orrery_games` does not pass the wall; the wall is why #804's
components hold the sum today.

### 3.2 What native would structurally fix: the schedule derivation

The genuinely structural yield is §2.4's schedule duplication. In Bevy,
data access *is* the system signature — a system over `Query<&mut Rock>`
declares its access, and the scheduler derives ordering and detects
ambiguity mechanically. That is precisely the derivation D43 (c)(1) asks
for and `mod.rs:886-892` admits is missing, and precisely the
"declaration → structural" yield #823 identified as its DSL's real benefit.
It is also, on the tree's own evidence, reachable without the dependency:
#823's `canonical_schedule!` emits the runnable schedule and the digested
declaration from one list, checked in `const` context.

Two qualifications, so the yield is not over-read:

- **The declaration does not disappear.** `CanonicalSchedule` ships to peers
  and D43 (g)'s digest covers it, so systems need wire-stable names whatever
  the substrate; Bevy's `type_name`-based system identity is not wire-stable.
  The yield is that the runnable/declared agreement becomes derived rather
  than test-held — one class of drift deleted, the stringly-typed surface
  kept.
- **The `SystemId` duplication between the macro call sites and the manifest
  stays** for the same reason.

### 3.3 What native would not fix even with the dependency taken

- **The neighbour-read surface** (§2.3) — the contract's shape, settled
  three times (#798 option A, #820's two divergences, the owner's standing
  constraint). A query would delete the recording; that is what a query is
  for.
- **The pair-shaped checks** (§2.2) — a question about section change is
  not expressible as a component signature.
- **The codecs** (§2.5) and **`materialize`/`deliver`** — canonical wire
  and executor contracts.
- **The invariant path** — `InvariantSample` is the witness's shape and is
  sum-typed by design; `section_invariant!` already gives it per-section
  signatures for the checks that want them.
- **Either capacity leg that matters.** §12.7's reversal (`docs/14-capacity.md:1516`)
  puts the ECS at **0.92× the executor at 24k entities — 85.4% of a 60 Hz
  budget**, where #777 recorded 104.8%: performance is no longer an argument
  for *or* against. The residual ECS weak legs are `state_bytes` at 1.15×
  and `collect_output_bytes` at 1.22×, and §12.7 pins them to the same seam
  line: *"Reading whole canonical states one id at a time is the ECS's weak
  leg, and it is the same `&R::CoreState` shape that stops the components
  holding a narrower payload."* Native does not move those numbers.

### 3.4 The residue classed

| Item | Location | Native fixes? | Retirable today? |
|---|---|---|---|
| `propagate_claim_overflow` four-arm | `mod.rs:768-777` | yes (trivially) | **yes** — two projected systems |
| `value_range` per-section halves | `invariants.rs:211-277` | yes | **yes** — `section_invariant!` |
| `value_range` monotonicity pair arms | `invariants.rs:278-334` | yes | yes — `sample.project::<Sec>()` |
| `Game::trajectory` | `mod.rs:1701-1708` | yes | yes — per-section on the host |
| `Body::from_state` | `visibility.rs:75-103` | yes | partly — only if reads leave `StateView` |
| neighbour let-elses | `visibility.rs:202`, `:212` | only by breaking the record | **no — contract** |
| `teleport` pair | `invariants.rs:124-152` | no | no — pair question |
| `value_range` discriminant arm | `invariants.rs:327-331` | no | no — pair question |
| codec dispatches + codecs | `state.rs:606-615`, `:617-707` … | no | no — wire contract |
| `materialize` / `deliver` | `mod.rs:632-684`, `:1580-1699` | no | no — executor contract |
| schedule declared/runnable duplication | `mod.rs:806-1039` | **yes — derived from signatures** | yes — #823's `canonical_schedule!` |
| ordering edges hand-written | `mod.rs:960-1039` | **yes — derived from signatures** | partly — #823 §6 |
| cap restated ×3, read-slot enum | `visibility.rs:47`, `mod.rs:172`, `:608` | no (not storage) | yes — #823's `recorded_reads!` |
| macro family | `mod.rs:14-85` | yes — no DSL needed | yes — it is already the minimal form |

The one column that is native-only is the schedule derivation. Everything
else in the "yes" column has a non-Bevy mechanism already in the tree or
already spiked.

---

## 4. What native would cost now, against today's tree

The record cost is already paid: #805 amended D42 (a), D42 (b)(2)/(d) and
D43 (e)(1), and `BEVY_PERMITTED_CRATES=(orrery_games)` exists
(`core-gates.sh:259`). What remains is code, and it is concrete:

1. **A rules-signature decision.** Either amend the D43 (e)(5) premise —
   #796's finding: `Ruleset::step` taking one entity's state plus a recorded
   neighbour view is what makes world-of-one replay *a type, not a
   convention*, and `Query<&mut Rock>` deletes it — or run the #820
   arrangement: the rules object owns the `World`, a mirror backend keeps it
   equal to the `Executor`'s store, and `step` drains the access log into
   `StateView::neighbor`. The second keeps every contract and costs **two
   stores in lockstep for the duration of every step**. #804's version of
   the same fork: *"Making the step per-section is #793's native-ruleset
   question."*
2. **`RegolithLocals` becomes a problem again.** `CraftScratch` carries the
   unquantized kinematic window and three tick-start premises per entity per
   tick (`mod.rs:710-742`). In Bevy these become either per-tick component
   insertion/removal (an archetype move per entity per tick — the exact cost
   `craft-load-kinematics`/`craft-store-kinematics` exist to avoid) or a
   resource-keyed map (worse than the current `cx.locals`). Regolith already
   paid this down once, from a 28-field destructure to in-place mutation
   (`mod.rs:692-705`); native re-bills it.
3. **Per-entity event order and locals reset must be reproduced exactly.**
   `StepCtx::emit` (`sched.rs:94-99`) gives ordered per-entity emission,
   locals reset at the top of each entity's tick (`mod.rs:707-709`), and the
   RNG is seeded per entity per tick (`canonical_step` → `tick_rng(seed,
   entity, tick)`). Bevy schedules order *systems*, not per-entity
   event sequences; the F-4 differential is the guard that would catch any
   drift.
4. **The seam wall itself is unchanged work**: widening `state` or making
   the step per-section means amending `orrery_core`, which needs its own
   owner decision and keeps the eleven-crate consumer set Bevy-free. Until
   then, a native ruleset in `orrery_games` runs on dual storage (item 1).
5. **Rollback numbers from #796 stand**: 9-tick window over 512 entities,
   2,826 µs on the `Executor` vs 2,830 µs on the ECS with byte-identical
   chains — but world *construction* 110 µs at N=512 (4.3× the window) with
   no `Clone for World` in bevy_ecs 0.19.1, and Regolith's population
   changes mid-window (splits and blooms; that is why `REGOLITH_WORLD_SECTIONS`
   is the migration frontier, `state.rs:465-475`).
6. **Consumers keep their obligations**: `orrery_sim`'s `#[repr(C)]` cdylib,
   `clients/regolith`, the gates; and `core-gates.sh` clause 5's neighbour
   scan becomes load-bearing rather than belt-and-braces (the acceptance
   comment already says so).

---

## 5. Is the answer different for Regolith than for a future game?

**The in-tree evidence says the future game is the *cheaper* native case —
and it is not hypothetical, it is Skirmish.**

Skirmish (`crates/orrery_games/src/skirmish/`) is a single-section ruleset:
`type CoreState = Craft` (`skirmish/mod.rs:359`), invariants written
directly over the component (`skirmish/invariants.rs:76`,
`pub const INVARIANTS: &[Invariant<Craft>]`), no `Section` machinery, no
section matches, no macro family — because there is nothing to project:

> One module, and the honesty of that is the reason Skirmish could be given
> a manifest inside #761 rather than waiting for a composition-root lane:
> there is no module split to get wrong. `Ruleset::CoreState` is a single
> `Craft`, every order and outcome in `order` concerns it.

(`skirmish/mod.rs:162-166`.) The entire §2 inventory is a *multi-section*
cost. A future single-section game pays none of it today, native or not.

Two honest asymmetries, in both directions:

**For a future game, native avoids costs that Regolith had to pay down:**

- The schedule idiom is Bevy-shaped already (`orrery_core::sched` takes "the
  projection and leaves the selection", `sched.rs:5-9`), so a Bevy-native
  game gets system tables, ambiguity detection and access derivation from
  the substrate instead of from `orrery_core`'s tables plus #823's macros.
- A game authored native from day one need not mirror a `BTreeMap` executor
  at all — the dual-store cost in §4 item 1 is a *migration* cost, not a
  native-from-scratch cost.

**And the contract is per-game, not per-Regolith:**

- The neighbour discipline, the cap accounting, the read recording, the
  digest, the codecs, the invariant path and the executor contracts bind
  every ruleset equally. What queries offer that the current idiom does not
  — population iteration inside the step — is exactly what D43 (e)(5) and
  the standing constraint forbid, for any game.
- The ergonomics the owner named ("the game code itself — queries in
  particular") are, for own-state code, already delivered to Regolith by
  `projected_system!` and to any future game by `orrery_core::sched` without
  the dependency. The one ergonomic item Bevy adds over that idiom is the
  schedule derivation (§3.2) — real, structural, and available either way.

So: Regolith's residual native-specific yield measures close to the empty
set, and the general yield concentrates in one structural item (schedule
derivation) plus an authoring-comfort preference that the tree has already
partly neutralized. A future game's case rests on what Regolith's cannot:
skipping migration costs and choosing the substrate once. That is an
argument about games not yet written, and it is the only place the case
still has mass.

---

## 6. Sources

- Tree (`cfbf607`): `crates/orrery_games/src/regolith/{mod,state,invariants,visibility,craft,world}.rs`;
  `crates/orrery_games/src/skirmish/mod.rs`, `skirmish/invariants.rs`;
  `crates/orrery_core/src/{executor,ruleset,invariants,sched}.rs`;
  `crates/orrery_sim_host/src/ecs.rs`; `scripts/core-gates.sh`;
  `crates/orrery_games/Cargo.toml` (no Bevy in `[dependencies]`).
- `docs/14-capacity.md` §12.7 (the reversal; the byte-read legs).
- Issues: #791 (the original match), #793 (this arc; the 2026-08-31
  acceptance and the 2026-09-01 correction), #796 (native spike; rollback
  numbers; the blocker), #798 (option A), #800 (enforced orders),
  #802 (seam widening), #804 (E0515; capacity re-run; the handoff),
  #805 (record amendments; the eleven-consumer count), #820 (OrderedQuery
  through replay; two divergences), #823 (macro DSL; declaration →
  structural), #826 (mandatory-freshness `StateView::new`).
- Records: D42 (a), (b)(2), (d); D43 (c)(1), (e)(1), (e)(5), (g).
