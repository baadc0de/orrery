# Spike: a curated `bevy_ecs` facade, and an `OrderedQuery` that logs both halves

**Status: spike, propose-only, non-normative.** Nothing here is wired into a
shipped crate, no existing crate is migrated, `scripts/core-gates.sh` is not
edited, and `bevy_ecs` is neither forked nor vendored. The deliverable is two
throwaway crates that compile — `spikes/orrery_ecs_facade` and
`spikes/facade_game` — plus this document.

**Date:** 2026-08-31. **Refs:**
[#793](https://github.com/baadc0de/orrery/issues/793) (owner acceptance,
2026-08-31), [#798](https://github.com/baadc0de/orrery/pull/798) (neighbour
access; chose option A), [#796](https://github.com/baadc0de/orrery/pull/796)
(native ergonomics), [#804](https://github.com/baadc0de/orrery/pull/804)
(storage layout), [#810](https://github.com/baadc0de/orrery/pull/810) (the
replicon wrapper, whose owner comment names the mechanism this spike
generalises), [#805](https://github.com/baadc0de/orrery/pull/805)
(`BEVY_PERMITTED_CRATES`).

**Sources.** Every line number below was read this session in this worktree,
except upstream citations, which are
`~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/` at the versions named.

---

## 0. The design under test, and the one-paragraph verdict

The owner's proposal:

> Maybe our ecs crate reexports `bevy_ecs` **SANS `Query`** and only exports
> `OrderedQuery` and other items that are kosher to tap in `bevy_ecs`. A
> compile-time gate checks that game crates don't try to smuggle in a
> `bevy_ecs` otherwise.

**It works, and the load-bearing unknown resolves in its favour: the derives do
follow the facade.** The mechanism is one manifest line, not a fork and not a
`#[ecs_path]` attribute. But two things are not as the proposal assumes.
First, *curating `Query` out is not the interesting part* — the interesting
part is that `#[derive(SystemParam)]` forces `World` into any facade that
admits it, and `World` is a universal door, so the facade must withhold
`SystemParam` from game crates entirely and define `OrderedQuery` on its own
side. Second, **a name allowlist is not sufficient**: a re-exported type
*alias* handed a game crate a live `DeferredWorld` by inference, with
`DeferredWorld` never named. That is measured below, not argued.

| # | Question | Answer |
|---|---|---|
| 1 | Do the derive macros follow the facade? | **Yes**, via a `package =` rename. No attribute, no fork. |
| 2 | Is `Query` the only door? | **No.** `World` is forced in by `#[derive(SystemParam)]`; a type alias leaks `DeferredWorld`. Allowlist below. |
| 3 | Can `OrderedQuery` be built without forking? | **Yes.** A plain `#[derive(SystemParam)]` in the facade. It compiles and logs both halves. |
| 4 | Does the log satisfy the replay contract? | **For `get`, yes** — same shape `replay.rs:325` compares. **For `enumerate`, no** — it is #798's option D and carries option D's bill. |
| 5 | What does the gate clause look like? | An inverted clause over `cargo metadata`, not `cargo tree`. Sketched and run in §5. |

---

## 1. Do the derive macros follow the facade?

### 1.1 The boundary, first, because it is what makes any of this enforcement

In Rust 2018+ a crate can only name a dependency it *declares*. A transitive
dependency is not in the extern prelude. Measured — a game crate depending on a
facade and not on `bevy_ecs`, with the facade re-exporting `Component`:

```
error[E0433]: cannot find module or crate `bevy_ecs` in this scope
 --> game/src/lib.rs:1:10
  |
1 | #[derive(facade::Component)]
  |          ^^^^^^^^^^^^^^^^^ use of unresolved module or unlinked crate `bevy_ecs`
  |
  = note: this error originates in the derive macro `facade::Component`
```

That error is both halves of the finding at once. The good half: **upstream
cannot be named**, so the facade is a visibility boundary and not a lint. The
bad half: **the derive expansion cannot be named either**, and unless that is
fixed the facade forces hand-written `impl Component`, which is exactly the
ergonomics this arc exists for.

### 1.2 How `bevy_ecs_macros` resolves its crate path

`bevy_ecs_macros` does not emit `::bevy_ecs`. Every derive calls
`bevy_ecs_path()` (`bevy_ecs_macros-0.19.1/src/lib.rs:527-529`):

```rust
pub(crate) fn bevy_ecs_path() -> syn::Path {
    BevyManifest::shared(|manifest| manifest.get_path("bevy_ecs"))
}
```

and `BevyManifest` reads the **calling crate's** `Cargo.toml`, located by
`CARGO_MANIFEST_DIR` (`bevy_macro_utils-0.19.1/src/bevy_manifest.rs:53-66`).
The resolution is `maybe_get_path` (same file, 84-113), and the load-bearing
line is 88:

```rust
let package = if deps.get(name).is_some() {
    return Some(Self::parse_str(&rust_name));
```

`deps.get("bevy_ecs")` is a lookup on the **dependency table's key**, not on the
package name. Cargo lets those differ:

```toml
bevy_ecs = { package = "orrery_ecs_facade", path = "../orrery_ecs_facade" }
```

Now the key is `bevy_ecs`, so the macro emits `bevy_ecs::component::Component`;
and the extern prelude binds `bevy_ecs` to the facade, so that path resolves to
the facade. **The same one line satisfies the macro and closes the boundary.**
That line is `spikes/facade_game/Cargo.toml`, and it is the whole trick.

Three details worth recording because they bound the answer:

* **The emitted path is relative, not absolute.** `get_path`'s fallback
  (`bevy_manifest.rs:121-124`) parses the bare string `bevy_ecs`, and line 89
  does the same on the hit path — no leading `::`. So resolution goes through
  the extern prelude, which is precisely why the rename works. It also means a
  local module named `bevy_ecs` would shadow it — harmless, since a crate can
  only shadow with something it can already name.
* **`bevy` is checked as a fallback key** (line 90). A game crate that declared
  the `bevy` umbrella would redirect the derives to `bevy::ecs` instead. The
  gate in §5 must therefore refuse `bevy` as well as `bevy_ecs`.
* **This is an upstream implementation detail with no stability guarantee.**
  There is no `#[ecs_path]` escape attribute in `bevy_ecs_macros` 0.19.1 — the
  derives' attribute lists are `component`, `require`, `relationship`,
  `relationship_target`, `entities` (`lib.rs:825-828`) and `system_param`
  (`lib.rs:241`). If upstream ever keys on package name instead of manifest
  key, the facade loses the derives and the fallback is hand-written impls.
  That is the single largest standing risk in this design.

### 1.3 What the derives actually need

Established by starting from an empty facade and adding exactly what the
compiler asked for, rather than by reading the macro and guessing — the reading
would have missed two items. Measured minimum for `#[derive(Component)]` plus
`#[derive(Resource)]`:

| Module | Items |
|---|---|
| `component` | `Component`, `ComponentCloneBehavior`, `ComponentId`, `DefaultCloneBehaviorBase`, `DefaultCloneBehaviorSpecialization`, `DefaultCloneBehaviorViaClone`, `Immutable`, `Mutable`, `RequiredComponentsRegistrator`, `StorageType` |
| `relationship` | `ComponentRelationshipAccessor`, `Relationship`, `RelationshipCloneBehaviorSpecialization`, `RelationshipTarget` |
| `resource` | `IsResource`, `Resource` |

The two a reading misses are `DefaultCloneBehaviorBase` and
`DefaultCloneBehaviorViaClone`: they are not emitted as type paths but as a
`use` inside an autoderef-specialization block
(`bevy_ecs_macro_logic-0.19.1/src/component.rs:304-305`), so a scan for
`#bevy_ecs::…::Type` does not see them. Their absence produced an `E0599`
naming a method, not a path — an error that looks nothing like a missing
re-export.

`entity` and `lifecycle` are **not** required — deleting each and rebuilding
`facade_game` is green. That matters twice over: it keeps `World` out (§2), and
it lets the facade drop `lifecycle`, which turned out to be a door (§2.3).

**Answer to question 1: the derives follow the facade, unconditionally, at the
cost of one manifest line and a hand-written module tree of three modules.**

---

## 2. `Query` is not the only door

### 2.1 What curation does close

With the facade above plus `OrderedQuery`, these are all `compile_fail`
doctests in `spikes/facade_game/src/lib.rs`, run by `cargo test -p facade_game`:

```rust
bevy_ecs::system::Query<'_, '_, &'static Rock>          // no `system` module
&bevy_ecs::world::World                                 // no `world` module
bevy_ecs::system::Commands<'_, '_>
bevy_ecs::world::EntityRef<'_>
bevy_ecs::world::unsafe_world_cell::UnsafeWorldCell<'_>
use ::bevy_ecs::system::Query;                          // upstream is unnameable
```

The last one is the boundary, not the curation: there is no spelling of
upstream available in a crate that does not declare it, so `QueryState`,
`EntityMut`, `&World` in an exclusive system and every other door are closed by
the same fact rather than one at a time.

### 2.2 The door that cannot be curated: `World`, forced in by `SystemParam`

`derive_system_param_impl` (`bevy_ecs_macros-0.19.1/src/lib.rs:251+`) emits
paths into `change_detection`, `query`, `system` **and `world`** — including
`&mut #path::world::World` in the generated `init`, because that is the trait
signature it is implementing. Measured, compiling the derive in `facade_game`
against a facade with no `world` module:

```
error[E0433]: cannot find `system` in `bevy_ecs`
error[E0433]: cannot find `world` in `bevy_ecs`
error[E0433]: cannot find `query` in `bevy_ecs`
error[E0433]: cannot find `change_detection` in `bevy_ecs`
```

The facade cannot satisfy that with a wrapper or a newtype: the generated impl
must match `SystemParam`'s signature, so `world::World` must be *the* `World`.
And `World` carries `get`, `entity` and `query` inherently. Measured against a
facade that exposed `world::{DeferredWorld, World}` and nothing else new — all
three compile in the game crate, with `Query` still uncurated and unnameable:

```rust
pub fn via_world_get(world: &World, e: Entity) -> Option<&Rock> { world.get::<Rock>(e) }
pub fn via_entity_ref(world: &World, e: Entity) -> Option<&Rock> { world.entity(e).get::<Rock>() }
pub fn via_world_query(world: &mut World) -> usize {
    let mut state = world.query::<&Rock>();
    state.iter(world).count()
}
```

**So the facade may hand game crates components, or `#[derive(SystemParam)]`,
but not both.** This spike takes the first: `SystemParam` is re-exported so the
refusal is visible, `world` is not, and `OrderedQuery` is written *inside* the
facade where `World` is already in reach. The consequence for game authors is
real and should be stated as a cost: **a game crate cannot define a system
param of its own.** Every new one is an addition to the facade, which is a
reviewable act — and also a chokepoint that has to be staffed.

### 2.3 The door a name allowlist does not close at all

`bevy_ecs::lifecycle::ComponentHook` is a type *alias*:

```rust
pub type ComponentHook = for<'w> fn(DeferredWorld<'w>, HookContext);
```

Re-exporting the alias — which looks inert, and which a reviewer scanning an
allowlist for the word `World` passes straight over — lets a game crate write
this, and **it compiled**:

```rust
const HOOK: bevy_ecs::lifecycle::ComponentHook = |mut world, ctx| {
    let hp = world.get::<Rock>(ctx.entity).map(|r| r.hp);      // another entity's component
    let _other = world.entity(ctx.entity).get::<Rock>();
};
```

The closure's first parameter is *inferred* to be a `DeferredWorld` from the
alias. The game crate holds one, and calls inherent methods on it, having never
named `DeferredWorld` — which the facade does not export, and which no import
was needed for.

**The rule this establishes, and the one this spike would ask to be normative:**

> An ECS facade's permitted surface must be closed under the types reachable
> through the *signatures* of the items on it — parameter types, return types,
> associated types, and the expansions of type aliases — not merely over the
> names it writes down. Inherent methods require no import, so possessing a
> value is possessing its whole API.

Applying that rule removes `lifecycle` from the facade. The cost is that
component hooks are unavailable to game crates; `#[derive(Component)]` itself
does not need them (§1.3), so what is lost is `#[component(on_add = …)]`, not
components. A `compile_fail` doctest holds the door shut.

### 2.4 The allowlist

Paths a game crate may name, in full. This is the artefact the brief asked for
— a list, on the model of D43 clause (e)'s host allowlist, where absence is the
refusal.

| Path | Why it is permitted |
|---|---|
| `Component`, `component::*` (the ten items in §1.3) | required by `#[derive(Component)]`; all inert — `ComponentId` is an index, `RequiredComponentsRegistrator` registers types, the clone-behavior items are marker specialization |
| `relationship::*` (four items) | required by `#[derive(Component)]`; yields `Entity` ids only, never component data |
| `Resource`, `resource::{IsResource, Resource}` | required by `#[derive(Resource)]`; `IsResource` is a sealing marker |
| `Entity`, `entity::{Entity, EntityMapper, MapEntities}` | an `Entity` is an index, not a capability: it reads nothing without something to look it up in, and the only such thing a game crate can name is `OrderedQuery` |
| `SystemParam` | re-exported deliberately so the refusal in §2.2 is a refusal and not an omission |
| `OrderedQuery`, `PersistKey`, `KeyIndex`, `AccessLog`, `Access`, `AccessKind` | the facade's own types (§3) |

Refused, and each is a `compile_fail` doctest or unreachable by the boundary:
`Query`, `QueryState`, `World`, `DeferredWorld`, `UnsafeWorldCell`, `Commands`,
`EntityRef`, `EntityMut`, `EntityWorldMut`, `lifecycle::*`, `system::*` other
than `SystemParam`, `world::*`, and upstream `bevy_ecs` under any name.

---

## 3. `OrderedQuery`, without a fork

`spikes/orrery_ecs_facade/src/lib.rs`. It is a plain `#[derive(SystemParam)]`
over upstream `Query` — the same construction
`crates/orrery_sim_host/src/ecs.rs:367` already uses for `SectionStore`, so the
shape is established in this tree:

```rust
#[derive(bevy_ecs::system::SystemParam)]
pub struct OrderedQuery<'w, 's, D: bevy_ecs::query::QueryData + 'static> {
    rows: bevy_ecs::system::Query<'w, 's, (&'static PersistKey, D)>,
    index: bevy_ecs::system::Res<'w, KeyIndex>,
    log: bevy_ecs::system::ResMut<'w, AccessLog>,
}
```

The two methods are the whole design:

```rust
pub fn get<'a>(&'a mut self, key: PersistId) -> Option<D::Item<'a, 's>> {
    self.log.push(key, AccessKind::Searched);          // ← before the outcome is known
    let entity = *self.index.by_key.get(&key)?;
    let (_, item) = self.rows.get_mut(entity).ok()?;
    self.log.push(key, AccessKind::Tapped);
    Some(item)
}

pub fn enumerate(&mut self) -> Vec<PersistId> {
    let mut keys: Vec<PersistId> = self.rows.iter().map(|(key, _)| key.0).collect();
    keys.sort_unstable();                              // ← canonical, not archetype
    for key in &keys { self.log.push(*key, AccessKind::Enumerated); }
    keys
}
```

Three properties, each deliberate:

* **The search is logged first.** This is `StateView::neighbor`'s discipline
  (`crates/orrery_core/src/ruleset.rs:176-201`) transposed. The three ways
  `get` returns `None` — unknown id, no matching row, row fails `D` — are
  indistinguishable in the log from each other and from a hit.
* **There is no `Result`.** `Option` and nothing else. #798's decisive finding
  is that `query.get(e)`'s `Result` discriminant is an existence bit obtained
  with the log untouched; here the log is written on line one, so the
  discriminant carries nothing the log does not already carry. There is also no
  `Deref` and no `IntoIterator`, because either would hand back the underlying
  `Query`.
* **`PersistId` orders the enumeration, not the archetype.** `Entity` is a
  generational index whose value depends on spawn order, so it is neither a
  thing a log may contain nor a thing an order may be defined by. `PersistKey`
  is a component holding the `PersistId`; `KeyIndex` is a
  `BTreeMap<PersistId, Entity>` resource, which is what makes a *search* a
  lookup rather than a scan — a scan is an enumeration wearing a lookup's
  clothes.

### 3.1 The worked example: a search that finds nothing

`spikes/orrery_ecs_facade/tests/ordered_query.rs`,
`a_search_that_finds_nothing_still_records_the_search`. A world holds rocks
`1, 2, 3`; game code (in `facade_game`, which cannot name `bevy_ecs`) asks for
id `9`:

```rust
pub fn read_named_neighbour(
    rocks: &mut bevy_ecs::OrderedQuery<&'static Rock>,
    wanted: PersistId,
) -> Option<u32> {
    rocks.get(wanted).map(|rock| rock.hp)
}
```

The caller receives `None`. The log holds:

```
[ Access { key: PersistId(9), kind: Searched } ]
neighbor_reads() == [ PersistId(9) ]
```

and for the found case (`wanted = 2`):

```
[ Access { key: PersistId(2), kind: Searched },
  Access { key: PersistId(2), kind: Tapped   } ]
neighbor_reads() == [ PersistId(2) ]
```

**`neighbor_reads()` is identical in shape in both cases.** That is the point:
*"I never saw the occluder"* costs the rule exactly what seeing it costs. #798
asserted the opposite for every query-shaped option, with
`option_b_leaks_absence_with_an_empty_log`; that assertion does not hold
against this type.

Six tests, all passing:

```
test a_search_that_finds_nothing_still_records_the_search ... ok
test a_search_that_finds_something_records_both_halves ... ok
test neighbor_reads_deduplicates_in_first_mention_order ... ok
test enumeration_is_persistid_ordered_under_permuted_insertion ... ok
test raw_query_iteration_follows_insertion_order ... ok
test enumeration_costs_one_recorded_read_per_entity_and_blows_the_cap ... ok
```

plus ten doctests in `facade_game`, of which eight are `compile_fail`.

`raw_query_iteration_follows_insertion_order` is the second direction, so the
sort is measured rather than decorative: two permuted insertion orders yield
*different* raw query iteration and the *same* `enumerate()`. That is D43
clause (e)(4)'s property held by the type instead of by the author remembering.

---

## 4. Does the logged access satisfy the replay contract?

The contract is `crates/orrery_core/src/replay.rs:322-327`:

```rust
let expected_reads: Vec<_> = neighbor_frames.iter().map(|(neighbor, _, _)| *neighbor).collect();
if outcome.neighbor_reads != expected_reads {
    return Err(ReplayError::NeighborFramesMalformed);
}
```

`outcome.neighbor_reads` is `view.recorded_reads().to_vec()`
(`executor.rs:387`), which is deduplicated (`ruleset.rs:197`) and in first-read
order (`ruleset.rs:208`); `executor.rs:388-419` turns each id into a
`NeighborFrame`, with an **empty payload** for one that was not found or was
stale, and `replay.rs:265-272` reconstructs `None` from that empty payload. The
absent case already round-trips.

`AccessLog::neighbor_reads()` produces exactly that shape — deduplicated, in
first-mention order — and a test pins it
(`neighbor_reads_deduplicates_in_first_mention_order`: searches `3`, `1`, `3`
yield `[3, 1]`).

**So the answer splits, and the split is the finding.**

* **`get`-shaped access: yes, with no change to the record.** A tick whose only
  accesses are `get`s produces a `neighbor_reads` byte-for-byte in the existing
  shape, capped by the existing `Ruleset::max_neighbor_reads`
  (`ruleset.rs:319`), replayed by the existing comparison. The recorded-read
  discipline is restored *as a type*, which is what D43 clause (e)(5) had and
  what #796 said a query deletes.
* **`enumerate`-shaped access: no, not without amending the record.** An
  enumeration records the population, so `neighbor_reads.len()` is the
  population.
  `enumeration_costs_one_recorded_read_per_entity_and_blows_the_cap` asserts
  it: eight occluders produce eight recorded reads, against Regolith's cap of
  three (`crates/orrery_games/src/regolith/mod.rs:608` →
  `visibility.rs:47`, one slot each for locker, rock and collision).
  `replay.rs:275-278` rejects a window carrying more frames than the cap, so
  adopting enumeration inside the step means raising the cap to a population
  bound — at which point the cap bounds nothing, and replay stops being
  O(recorded reads) and becomes O(world). This is #798's option D, and #798
  priced it at 2.3× the shipped scenarios and ~170× at N=512.

**A conservative reading, which this spike recommends:** ship `get`, and treat
`enumerate` as a separate owner decision that is *not* implied by adopting the
facade. The type having an `enumerate` method at all is the honest way to say
"if you want this, here is what it costs" rather than "you cannot have this" —
but the cheapest correct posture is that `enumerate` stays unused inside the
canonical step, exactly where Regolith's `collision_candidate` broad phase
(`crates/orrery_games/src/regolith/mod.rs:556-569`) already lives: outside,
where the answer can only propose.

### 4.1 What is not demonstrated

Stated plainly, because this is the same gap #798 declared:

* ~~**Nothing is wired through `Ruleset::step`, `Executor`, `EcsBackend` or
  `ReplayHarness`.** The `get` compatibility is a demonstrated *shape* match
  against quoted code, not a replay that ran.~~ **Closed by §9.** A real
  `Ruleset` now runs on `Executor` and is adjudicated by `ReplayHarness`. The
  `get` result held; two other things did not.
* ~~**`OrderedQuery` does not implement staleness or #758's future-stamp
  refusal.**~~ **Closed by §9.3**, and it was not merely missing — §9.2 shows
  it convicting an honest authority. `ReadWindow` now carries it.
* **No `Ruleset` is written against it**, so nothing measures the ergonomics
  the arc is for. #796's before/after is the evidence for that and it is
  unaffected either way.
* **Log growth is measured in ids, not bytes**, and the cap comparison uses a
  population of eight because that is enough to cross a cap of three. #798's
  measured multipliers are the number to cite, not this test.

---

## 5. The gate clause

The existing clause 1 (`scripts/core-gates.sh:259-279`) asks *"is there Bevy
anywhere in this crate's dependency **graph**?"* and skips
`BEVY_PERMITTED_CRATES`. **That question cannot be inverted with the same
instrument.** Under a facade, upstream `bevy_ecs` is in every game crate's
graph transitively — measured: `cargo tree -p facade_game` prints 11 lines
matching `bevy`, on the crate that is the *proof* the boundary works. So a
`cargo tree` grep fires on exactly the crates it should pass.

The right question is one level shallower — *which crates **declare**
`bevy_ecs` directly, whatever they call it?* — and `cargo metadata --no-deps`
answers it, because it reports each declared dependency's real package `name`
alongside the manifest key as `rename`. Measured on this workspace:

```
orrery_sim_host   [('bevy_ecs', None, None), …]
orrery_ecs_facade [('bevy_ecs', None, None), …]
facade_game       [('orrery_ecs_facade', 'bevy_ecs', None), …]
```

The rename is visible and the package name is the truth, so a dependency
smuggled in under a different key is still reported as `bevy_ecs`.

**Sketch — not wired into `scripts/core-gates.sh`, per this spike's scope:**

```bash
# The only crates that may declare bevy_ecs (or the `bevy` umbrella) directly.
# Adding a row is an amendment to D42 (a) / D43 (e)(1), the same standard the
# existing BEVY_PERMITTED_CRATES list carries.
readonly BEVY_DECLARERS=(
  orrery_ecs_facade
  orrery_net orrery_authority orrery_witness orrery_spatial
  orrery_sim_host orrery_persist_client orrery_predict orrery
  aeronet_iroh aeronet_tokio_runtime bevy_replicon
)

# Captured into a variable first, deliberately: inside a process substitution a
# `cargo metadata` failure cannot fail the script, and a gate that exits 0
# because its instrument broke is worse than no gate.
meta="$(cd "$ROOT" && cargo metadata --format-version 1 --no-deps 2>/dev/null)" \
  || die "cargo metadata failed"

while read -r crate dep kind rename; do
  [[ $dep == bevy_ecs || $dep == bevy ]] || continue
  [[ $kind == normal ]] || continue
  printf '%s\n' "${BEVY_DECLARERS[@]}" | grep -Fxq "$crate" && continue
  if [[ $rename == "-" ]]; then
    die "$crate declares $dep directly; depend on orrery_ecs_facade instead"
  else
    die "$crate declares $dep directly under the key '$rename'; depend on orrery_ecs_facade instead"
  fi
done < <(printf '%s' "$meta" | python3 -c '
import json,sys
for p in json.load(sys.stdin)["packages"]:
    for d in p["dependencies"]:
        print(p["name"], d["name"], d["kind"] or "normal", d.get("rename") or "-")
')
```

Run against this worktree it exits 0. Given a deliberate break — a line
`smuggled = { package = "bevy_ecs", version = "0.19" }` added to
`spikes/facade_game/Cargo.toml` — it exits 1 with:

```
core-gates: facade_game declares bevy_ecs directly under the key 'smuggled';
depend on orrery_ecs_facade instead
```

The break was reverted; `spikes/facade_game/Cargo.toml` carries no `smuggled`
entry.

Note the `bevy` arm is not decoration: `maybe_get_path` falls back to the key
`bevy` (`bevy_manifest.rs:90`), so a game crate declaring the umbrella would
redirect the derives to `bevy::ecs` and get the whole engine with them.

Note also what the clause is: **the existing clause-1 machinery inverted**, not
new machinery. #805 already gave clause 1 an allowlist; this is the same
allowlist shape with `cargo tree` swapped for `cargo metadata` because the
question moved from graph to manifest.

---

## 6. Does this change #798's verdict?

**Its reasoning, yes. Its recommendation, partly — and the part that changes is
the part #804 already put in question.**

#798 chose option A on a decisive finding:

> A recorded read is a **lookup, not a dereference**. […] Every query-shaped
> alternative splits finding from reading, and `query.get(e)`'s `Result`
> discriminant is an existence bit obtained with the log untouched.

The second sentence is **true of `Query` and false as a general claim about
query-shaped types**. `OrderedQuery::get` does not split finding from reading:
it logs the search on its first line, before it knows anything, and returns
`Option` rather than `Result`. §3.1 shows the log is identical in shape for a
hit and a miss. So the argument that killed #798's option B — *"B moves the
guarantee from the lookup to the dereference and drops the half that
matters"* — does not reach this design, because this design keeps the guarantee
at the lookup and adds the dereference as a second record.

Two of #798's scoring rows change accordingly, for the `get`-shaped half:

| Row | #798, options B–D | This design (`get` only) |
|---|---|---|
| Type or convention? | convention for absence (B) | **type** — the search is unconditional and the caller has no way to look without recording |
| Enumeration leakage | open (B) | **closed**, and closed the way A closes it: the shape is not writable |
| Replay cost | O(reads) … O(population) for D | **O(reads)**, capped by the existing `max_neighbor_reads` |

What does **not** change:

* **On enumeration inside the step, #798 is simply right and this spike agrees**
  (§4). `enumerate` is option D with option D's bill, measured here as well.
* **#798's judgement that `neighbor(id)` is cheap and works** is untouched.
  Nothing here argues the shipped call site (`visibility.rs:171`, still the one
  production neighbour read) should move.

The thing that genuinely reopens the question is not this spike's cleverness,
it is #804's observation, which this spike makes concrete in four lines of
`facade_game`: `#[derive(Component)] struct Rock` compiles, in a crate that
cannot name `bevy_ecs`. #798's "C is A" row rested on `Query<&Rock>` not being
writable. It is writable now — as `OrderedQuery<&Rock>` — and the owner
acceptance of 2026-08-31 already admits the components that make it so. So the
honest statement is:

> #798's verdict was correct for the world it was written in, where
> `orrery_games` had no `bevy_ecs` dependency and the only query-shaped option
> on the table leaked absence. Both premises have since changed. The decisive
> objection does not apply to `OrderedQuery::get`, and the cost objection still
> applies, undiminished, to `OrderedQuery::enumerate`.

This is a spike and it proposes nothing normative. D42 clauses (a) and (b)(2)
and D43 clauses (e)(1), (e)(4) and (e)(5) are the owner's, and the standing
constraint recorded in #793 — *"rules reach neighbours through
`StateView::neighbor` and nowhere else"* — is unamended by anything here.

---

## 7. What would still be unenforced

The list the brief asked for, and the reason it is the most important section.

1. **The facade crate itself is unconstrained.** It declares real `bevy_ecs`
   and can do anything. Every line of it is review surface with no gate behind
   it, and it is the single point where a careless `pub use` reopens every
   door at once. The `lifecycle` alias (§2.3) is the proof that this is not
   hypothetical: it was in the first draft, it looked inert, and it leaked a
   `DeferredWorld`.
2. **Nothing mechanically checks the closure rule.** §2.3's rule — closed under
   reachable types, not just names — is applied by hand. A future addition that
   violates it fails no test. A `compile_fail` corpus catches the doors someone
   thought to write a test for, which is the same guarantee a review gives, one
   step earlier.
3. **The gate checks manifests, not conduct.** It refuses a crate *declaring*
   `bevy_ecs`; it says nothing about what a permitted declarer does. The
   boundary is exactly as good as the crate split, and code moves between
   crates.
4. **Recording is not honesty.** `OrderedQuery` records what a rule asked for.
   It does not oblige a rule to ask, and it cannot stop a rule inferring
   something about the world from its own state. What it removes is one
   specific shape: learning something about another entity while the log stays
   empty.
5. **Own-state components are unpoliced by construction.** A game crate can
   define any component it likes; the facade's discipline is about *access*,
   not about what may be stored. If canonical state ends up mirrored into a
   component that a system then reads across entities via `OrderedQuery`, the
   read is logged — but the modelling error that made it possible is not
   something a facade can see.
6. **`#[derive(SystemParam)]` is unavailable in game crates** (§2.2). Every new
   access shape must be added to the facade. That is a feature for review and a
   tax on authors, and if the tax is ever paid by re-exporting `world`, the
   entire design collapses to advisory in one line.
7. **Upstream can withdraw the mechanism.** §1.2: `BevyManifest` keying on the
   manifest key is undocumented behaviour. A bevy release that keys on package
   name instead breaks the derives for every game crate at once. The fallback
   is hand-written `impl Component`, which is the outcome this whole arc exists
   to avoid.

---

## 8. What was built

| Path | What it is |
|---|---|
| `spikes/orrery_ecs_facade/src/lib.rs` | the curated facade and `OrderedQuery`; declares real `bevy_ecs` |
| `spikes/orrery_ecs_facade/tests/ordered_query.rs` | six tests: the worked absent-search example, the replay-shape bridge, both directions of the ordering claim, the cap bill |
| `spikes/facade_game/Cargo.toml` | the one load-bearing line: `bevy_ecs = { package = "orrery_ecs_facade", … }` |
| `spikes/facade_game/src/lib.rs` | game-authored components and systems, plus eight `compile_fail` doctests |

Both are root-workspace members rather than a standalone workspace, on purpose:
a directory declaring its own `[workspace]` and absent from `check.sh`'s lane
table fails `check.sh --self-test` (`scripts/check.sh:695-708`), and a spike may
not edit that table.

Not done, deliberately: no crate is migrated onto the facade, `orrery_games` is
untouched, `scripts/core-gates.sh` is untouched, no ADR is amended, and
`bevy_ecs` is neither forked nor vendored.

---

## 9. The replay that ran (#815's stated gap, closed)

§4.1's first bullet said the `get` compatibility was *"a demonstrated shape
match against quoted code, not a replay that ran"*. This section is the replay
that ran. Everything below is from
`spikes/orrery_ecs_facade/tests/replay_through_ordered_query.rs`, eight tests,
all green, against unmodified `orrery_core`.

### 9.0 The wiring, and the one thing it could not do

`Ruleset::step` (`crates/orrery_core/src/ruleset.rs:338`) is handed a
`&mut StateView<'_, CoreState>` and nothing else. `StateView`'s neighbour
snapshot is a private `&BTreeMap` (`ruleset.rs:105`) reachable only through
`StateView::neighbor` (`ruleset.rs:176`) — which is itself the recording call.
**A query therefore cannot be sourced from a `StateView`.** That is a hard
finding about the current signature and it shapes everything else here.

What can be done without amending `orrery_core` is the inverse:

1. the rules object owns the `World` (behind a `Mutex`, because `step` takes
   `&self`);
2. a `MirrorBackend: TickBackend` keeps that `World` equal to `Executor`'s
   store, mirroring every `insert_observed`, `take_state` and post-step
   write-back;
3. `step` runs the game's rule through `OrderedQuery` against the `World`;
4. `step` then **drains** `AccessLog::neighbor_reads()` into
   `StateView::neighbor`, one call per id, in order.

`StateView` stays the ledger; `OrderedQuery` becomes the read path. This is the
*most favourable* arrangement available to the design — the two stores are
equal by construction, and the recorded sequence is produced by the query
itself. Every divergence below survives it.

The drain is load-bearing and measured as such: deleting the
`view.neighbor(key)` loop fails seven of the eight tests.

### 9.1 The result: `get` produces the sequence `replay.rs:325` accepts

The rule consults `[78, 999, 78]` each tick — a hit, an id installed nowhere,
and the hit again. Over a three-tick signed window with one `LogFrame`,
chain-folded and signed, replayed on a fresh `MirrorBackend` through
`ReplayHarness::on`:

| | tick T₀ | T₀+1 | T₀+2 |
|---|---|---|---|
| rule consulted | 78, 999, 78 | 78, 999, 78 | 78, 999, 78 |
| `AccessLog::neighbor_reads()` | `[78, 999]` | `[78, 999]` | `[78, 999]` |
| `outcome.neighbor_reads` | `[78, 999]` | `[78, 999]` | `[78, 999]` |
| `NeighborFrame` ids logged (`expected_reads`) | `[78, 999]` | `[78, 999]` | `[78, 999]` |
| frame `present` bits | `[true, false]` | `[true, false]` | `[true, false]` |

`outcome.neighbor_reads == expected_reads` at `replay.rs:325` on every tick,
and **every tick reproduces the state hash the authority claimed**. Dedup,
first-mention order and the absent case all round-trip. #815's claim about
`get` is confirmed by execution, not by shape.

The check is not vacuous. `a_permuted_frame_order_is_refused` swaps the two
neighbour records' contents within one tick, re-signs the frame, and gets
`Err(NeighborFramesMalformed)` — so the green result above is about ordering.
(Swapping whole records instead trips the earlier `seq` legality check and
returns `InputOrderIllegal`, which is a different fact; the test avoids it
deliberately.)

### 9.2 Two refutations, and the second is the serious one

**(a) The query answers for the reader's own id.** `StateView::neighbor` refuses
it by identity (`ruleset.rs:176`, `id != self.entity`) while still recording the
ask. `OrderedQuery` — as #815 left it — has no idea who is reading, so it
returns the row.

The read is recorded either way, so `replay.rs:325` is satisfied and **the
window adjudicates clean**. What the log then says is false: `canonical_step`
writes the own-id frame `present: false` with an empty payload
(`executor.rs:394`, `(*neighbor != entity).then(..)`), while the rule consumed
the entity's own state through a path the evidence does not describe. Measured:
own `hp` 100, neighbour `hp` 7; a genuine miss then a hit gives 106, and the
run gives **207**.

**(b) A stale read convicts an honest authority.** This is the one that
matters. `StateView::neighbor` hides an observation older than
`max_neighbor_staleness_ticks` (`ruleset.rs:192`, `checked_sub`); `canonical_step`
then logs the frame as `present: false` stamped at the *reader's* tick
(`executor.rs:394-415`). `OrderedQuery` performs no such check — the row is in
the `World`, so it is returned.

So the authority computes with state its own log says was never delivered, and
the adjudicator, which installs only `present` frames, computes without it:

| | authority | adjudicator |
|---|---|---|
| neighbour observed at | T−10, cap 5 | frame says `present: false` |
| query returned | `Some(hp = 7)` | `None` |
| logged sequence | `[78, 999]` | `[78, 999]` — **matches** |
| state hash | *h* | *h′ ≠ h* |

The window is *well-formed*. The sequence check passes. The hashes diverge, and
in `verify_bundle` terms that is a `Confirms` against an authority that did
nothing wrong. A refusal would have been safe; this is not.

### 9.3 The gap, closed: `ReadWindow`

Both refutations are the same missing fact — the query does not know **who** is
reading or **when**. Neither is derivable inside a rule: `Ruleset::step` is
handed no tick at all, and the reader's identity reaches a rule only through
`StateView::entity`. So it is a *host* obligation, and the facade now models it:

```rust
#[derive(Component)] pub struct ObservedAt(pub Tick);

#[derive(Resource)] pub struct ReadWindow { /* reader, tick, max_staleness_ticks */ }
impl ReadWindow {
    pub const fn observed(reader: PersistId, tick: Tick, max_staleness_ticks: u64) -> Self;
    pub const fn open() -> Self; // #815's behaviour, kept runnable for the refutations
}
```

`ReadWindow::observed`'s three arguments are exactly `StateView::observed`'s
`entity`, `tick` and `staleness_cap` (`ruleset.rs:124`), and `admits` is the
same `checked_sub` for the same #758 reason: a stamp *ahead* of the reader is
state from the reader's future, which `ReplayHarness` already refuses
(`replay.rs:259`). `OrderedQuery::get` now records `Searched`, then refuses on
identity, then refuses on staleness — so a hidden row and an absent row are
still indistinguishable in the log.

With the host stamping it (`MirrorBackend::step_entity` does, before calling
`Executor::step_entity`, because the tick is not in the rule's signature):

* the stale window of §9.2(b) replays to **the same hashes**;
* the own id is refused, recorded, and priced as a miss (100 → 106);
* an observation *inside* the cap is still delivered (four ticks back against a
  cap of five → 107), so the bound is a bound and not a switch.

`ReadWindow::open()` is retained only so the two refutations stay runnable
measurements rather than recollections. **No host may ship it.**

### 9.4 Can a game crate be written with no `World` in reach?

**Yes — for the rule. No — for the system.** The whole rule is
`facade_game::erode`:

```rust
pub fn erode(own: &mut Rock, rocks: &mut bevy_ecs::OrderedQuery<&'static Rock>, targets: &[PersistId])
```

No `World`, no `Commands`, no `Res`, no `SystemParam` derive, in a crate where
`bevy_ecs` *is* the facade and upstream is not in the extern prelude at all.
§2.2's resolution holds.

But the line falls in a specific place, and it is not where "game crates never
derive `SystemParam`" suggests. The **system** — the thing with `Res<Targets>`
and `ResMut<Own>` in its signature, the thing bevy actually runs — is written on
the *host* side, because `Res` and `ResMut` are not on the curated surface.
The game contributes a plain function that the host's system calls. So:

* a game crate **cannot declare a resource**, cannot schedule, cannot express
  ordering between its own systems, and cannot add a new access shape;
* every one of those is a host edit, reviewed as a facade change. That is the
  tax §7(6) named, now priced concretely: it is not "add a param to the
  facade", it is "the host owns every system signature a game will ever need".

For the rule itself, what the facade must provide is short:

| Item | Why |
|---|---|
| `Component`, `Resource`, `Entity` and their derive support | already §2.4's allowlist |
| `OrderedQuery<D>` with `get` | the only neighbour read path |
| own state as `&mut C` — the *same* component type the query yields | so a rule never learns own state is stored differently |
| `PersistKey`, `KeyIndex`, `ObservedAt`, `ReadWindow`, `AccessLog` | host-side plumbing the game may name but cannot usefully act on |

Audited against §2.3's closure rule — *closed under the types reachable through
the signatures of listed items* — the items added by this lane pass:
`KeyIndex::entity` returns `Entity`, which was already exported and is inert
without something to look it up in; `ObservedAt` and `ReadWindow` are built from
`Tick`, `PersistId` and `u64`; `OrderedQuery`'s public methods return
`D::Item`, `Vec<PersistId>` and `&AccessLog`. No `World`, `DeferredWorld`,
`EntityRef` or `UnsafeWorldCell` is reachable through any of them, and
`facade_game`'s existing `compile_fail` doctests still refuse all four.

One caveat, recorded rather than resolved: `SystemParam` remains re-exported
(§2.2 explains why), so its trait methods are nameable in a game crate. They are
not *callable* — `init_state` needs a `&mut World` value, which a game crate
cannot construct or obtain — but this is an argument, not a `compile_fail`, and
it is the kind of argument §2.3 exists to distrust.

### 9.5 What is still unproven

Same standard as §4.1, and the list is not short.

* **The `World`-in-a-`Mutex` is a spike's answer to a signature problem, not a
  design.** `Ruleset::step` is pure by contract (VC-8); this implementation
  reads state that did not arrive through `view`. It produces the right bytes
  here because `MirrorBackend` keeps the two stores equal, and that equality is
  an invariant of ~40 lines of test code, not of anything shipped. **A real
  bevy-native path must change `Ruleset::step`'s signature**, and this lane did
  not design that change.
* **The drain is not the design either.** In the real thing, `OrderedQuery`'s
  log *is* the neighbour record; here it is copied into `StateView`, which
  means `StateView::neighbor`'s freshness filter runs a second time over ids
  the query already decided about. Two implementations of one rule agreed in
  every case tested; nothing proves they must.
* **One entity, one neighbour, one component type, three ticks, no events, no
  materialization, no RNG draw.** The window is the smallest one that can carry
  a `NeighborFrame`. Nothing here exercises `step_tick`, multi-entity ordering,
  `EcsBackend`, or `orrery_sim_host` at all.
* **`enumerate` is untouched by this lane** and still carries §4's bill. It now
  also filters on `ReadWindow`, which is untested against replay.
* **The cap interaction is unexercised.** `max_neighbor_reads` is 4 against 2
  distinct reads; `replay.rs:275-278`'s cap rejection never fires in these
  tests.
* **`ReadWindow` is stamped by one host, in one place.** Nothing enforces that a
  host stamps it, and `ReadWindow::open()` — which must never ship — is a public
  constructor. A production facade would make the window a construction
  parameter rather than a resource anyone may overwrite; that is a design this
  lane names and does not build.
* **No ADR is amended and none is proposed.** What is owed, if this path is
  taken: D43 (e)(5) currently reads as though `StateView` is the only recorded
  read path, and §9.3 makes `ReadWindow` a second place the staleness bound is
  implemented. Two implementations of one normative rule is exactly the shape
  ADR-0043 clause (b) exists to prevent.

### 9.6 What was built (this lane)

| Path | What it is |
|---|---|
| `spikes/orrery_ecs_facade/tests/replay_through_ordered_query.rs` | the `Ruleset`, the `MirrorBackend`, the signed window, and eight tests |
| `spikes/orrery_ecs_facade/src/lib.rs` | `ObservedAt`, `ReadWindow`, `KeyIndex::entity`, and the two refusals inside `get` |
| `spikes/facade_game/src/lib.rs` | `erode` — the whole rule, with no `World` in its signature — and `Rock`'s `CoreCodec`/`Quantized` |

Not done, deliberately: Regolith is not migrated, `orrery_core` is not amended,
`scripts/core-gates.sh` is untouched, no ADR is amended, and `bevy_ecs` is
neither forked nor vendored. `Ruleset` is implemented in `tests/`, not `src/`,
so `core-gates.sh`'s role discovery — which reads library sources only
(`scripts/core-gates.sh:94-102`) — does not gate this crate.
