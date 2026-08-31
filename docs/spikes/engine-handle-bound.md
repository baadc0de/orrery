# Spike: can `bevy_replicon`'s registration API carry an `EngineHandleFree` bound without a fork?

**Status:** completed read-and-report spike. **Issue:** #638. **Date:** 2026-08-31.

This spike answers exactly the question A9 §9 left unevidenced: whether the
vendored `bevy_replicon` copy at `vendor/bevy_replicon` admits an extra trait
bound on its payload-registration path, or whether enforcing one requires
forking that vendored crate. It does **not** choose between the compile-time
and registry-time mechanisms (owner-reserved OD-26), and it does **not**
implement the marker.

---

## 1. Verdict

**The bound can be added downstream through a wrapper. If the wrapper is the
only declarer of `bevy_replicon` and a gate enforces that, the wrapper is
unbypassable without a manifest change that CI can refuse. A fork is required
only if the owner wants the bound non-bypassable inside the type system itself
(i.e. even when a crate legitimately holds a direct `bevy_replicon`
dependency).**

Stated more precisely: `bevy_replicon`'s own registration entry points do not
require, and cannot be made to require from outside, a sealed
`EngineHandleFree` marker. A first-party extension trait that forwards to those
entry points can impose the bound at every Orrery-owned call site, and a
component carrying `Entity` (or any other engine handle) then fails to compile
at that wrapper call site. The underlying replicon methods remain directly
callable *by any crate that can name `bevy_replicon`*, but in Rust 2018+ a crate
can only name dependencies it declares. A wrapper that is the sole declarer
therefore turns a transitive dependency into a visibility boundary.

The bypass then requires adding `bevy_replicon` to a crate's `Cargo.toml`. That
is a reviewable act, and it is mechanically refusable by extending the existing
per-crate `cargo tree` scan in `scripts/core-gates.sh` clause 1 with a small
allowlist/denylist. That is not identical to a fork's guarantee — a fork makes
the bound unbypassable within the type system, while the gate makes it
unbypassable without a manifest change that a gate refuses — but it is enforced
by tooling rather than by review, which is the bar #638 is actually trying to
clear.

---

## 2. Evidence from `vendor/bevy_replicon`

Read and quoted; no inference from documentation.

### 2.1 The high-level `App` entry points live in `AppRuleExt`

`vendor/bevy_replicon/src/shared/replication/rules.rs`:

```rust
// lines 23-28
fn replicate<C>(&mut self) -> &mut Self
where
    C: Component<Mutability: MutWrite<C>> + Serialize + DeserializeOwned,
{
    self.replicate_filtered::<C, ()>()
}
```

```rust
// lines 774-776
fn replicate_with<R: IntoComponentRules>(&mut self, component_rules: R) -> &mut Self {
    self.replicate_with_filtered::<_, ()>(component_rules)
}
```

```rust
// lines 896-898
fn replicate_bundle<B: BundleRules>(&mut self) -> &mut Self {
    self.replicate_bundle_filtered::<B, ()>()
}
```

These are trait methods with default bodies. The bounds are fixed in the
vendored source; a downstream crate cannot tighten them.

### 2.2 The rule-to-registry conversion is in `IntoComponentRule`

`vendor/bevy_replicon/src/shared/replication/rules/component.rs`:

```rust
// lines 60-65
impl<C: Component<Mutability: MutWrite<C>>> IntoComponentRule for RuleFns<C> {
    fn into_rule(self, world: &mut World, registry: &mut ReplicationRegistry) -> ComponentRule {
        let (id, fns_id) = registry.register_rule_fns(world, self);
        ComponentRule::new(id, fns_id)
    }
}
```

There is no hook here for an extra bound; the impl is unconditional over
`C: Component<Mutability: MutWrite<C>>`.

### 2.3 `RuleFns` constructors have no handle-related bound

`vendor/bevy_replicon/src/shared/replication/registry/rule_fns.rs`:

```rust
// lines 88-95
impl<C: Component> RuleFns<C> {
    pub fn new(serialize: SerializeFn<C>, deserialize: DeserializeFn<C>) -> Self {
        Self {
            serialize,
            deserialize,
            deserialize_in_place: in_place_as_deserialize::<C>,
            consume: consume_as_deserialize,
        }
    }
}
```

```rust
// lines 179-186
impl<C: Component + Serialize + DeserializeOwned> Default for RuleFns<C> {
    fn default() -> Self {
        Self::new(default_serialize::<C>, default_deserialize::<C>)
    }
}
```

A downstream wrapper can refuse to construct or expose `RuleFns<C>` for handle
schemas, but `RuleFns::<Entity>::default()` compiles as far as replicon is
concerned.

### 2.4 The registry accepts any `C: Component<Mutability: MutWrite<C>>`

`vendor/bevy_replicon/src/shared/replication/registry.rs`:

```rust
// lines 108-119
pub fn register_rule_fns<C: Component<Mutability: MutWrite<C>>>(
    &mut self,
    world: &mut World,
    rule_fns: RuleFns<C>,
) -> (ComponentId, FnsId) {
    let (index, component_id) = self.init_component_fns::<C>(world);
    self.rules.push((index, rule_fns.into()));
    let fns_id = FnsId(self.rules.len() - 1);

    trace!("registering `{fns_id:?}` for `{}`", ShortName::of::<C>());
    (component_id, fns_id)
}
```

This is the final registration seam. It knows the component type only through
the `Component` and `MutWrite` traits.

---

## 3. The wrapper sketch, compiled and verified

A throwaway integration test in `crates/orrery_spatial/tests/` was used to
confirm that a downstream extension trait can add the bound. The file was
deleted and the `Cargo.toml` change was reverted before this document was
written; `git status` is clean.

The sketch (slightly condensed from the compiled version):

```rust
use bevy::prelude::*;
use bevy_replicon::{
    prelude::*,
    shared::replication::registry::receive_fns::MutWrite,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

mod sealed {
    pub trait EngineHandleFree {}
    impl EngineHandleFree for u8 {}
    impl EngineHandleFree for u64 {}
    impl<T: EngineHandleFree> EngineHandleFree for Vec<T> {}
    // Deliberately not implemented for `Entity`, `ComponentId`, etc.
}

pub trait OrreryReplicateExt {
    fn orrery_replicate<C>(&mut self) -> &mut Self
    where
        C: Component + Serialize + DeserializeOwned + sealed::EngineHandleFree,
        C::Mutability: MutWrite<C>;
}

impl OrreryReplicateExt for App {
    fn orrery_replicate<C>(&mut self) -> &mut Self
    where
        C: Component + Serialize + DeserializeOwned + sealed::EngineHandleFree,
        C::Mutability: MutWrite<C>,
    {
        self.replicate::<C>()
    }
}

#[derive(Component, Serialize, Deserialize)]
struct Good(u64);
impl sealed::EngineHandleFree for Good {}

#[derive(Component, Serialize, Deserialize)]
struct Bad(Entity);
```

Results:

- `app.orrery_replicate::<Good>()` compiled cleanly.
- Adding `app.orrery_replicate::<Bad>()` produced the expected refusal:

```text
error[E0277]: the trait bound `Bad: EngineHandleFree` is not satisfied
  --> crates/orrery_spatial/tests/engine_handle_spike.rs:52:28
    |
52 |     app.orrery_replicate::<Bad>();
    |         ----------------   ^^^ unsatisfied trait bound
```

So the compile-time mechanism is technically feasible without touching the
vendored crate, **provided the registration seam is owned by Orrery**. The
bound attaches at the wrapper's `where` clause, and the rejection happens at the
call site before any replicon code is invoked.

---

## 4. The third option: wrapper + dependency gate

The original verdict treated the wrapper as advisory because a caller could
"simply import" `bevy_replicon` and call the vendored method directly. That
bypass is real only for crates that can name `bevy_replicon`. In Rust 2018+ a
crate can only name dependencies it declares; a transitive dependency is not in
the extern prelude.

This was verified empirically with a minimal three-crate workspace: a `wrapper`
crate declares an `engine` crate and re-exports its type; a `consumer` crate
depends only on `wrapper`. A `use engine::Thing` in `consumer` fails with
`E0433: cannot find module or crate 'engine'`, while `use wrapper::Thing`
compiles. The same shape applies to `bevy_replicon`.

### 4.1 Only two workspace crates declare `bevy_replicon`

Parsing every workspace manifest with `cargo metadata` and `tomllib` confirms
that exactly two library crates list `bevy_replicon` as a dependency, both in
the non-dev `[dependencies]` table:

- `crates/orrery_spatial/Cargo.toml:29`
- `crates/orrery_persist_client/Cargo.toml:37`

No other workspace crate can name `bevy_replicon` today, including
`orrery_games` (`crates/orrery_games/Cargo.toml` lists no replicon dependency).

### 4.2 How the gate would work

The wrapper crate becomes the only workspace member that declares
`bevy_replicon`. `orrery_spatial` and `orrery_persist_client` remove their
`bevy_replicon` dependency and depend on the wrapper instead. The wrapper
re-exports the registration traits and the non-registration items those two
crates currently reach for directly:

- from `orrery_spatial`: `prelude::*`, `server::visibility::*`,
  `shared::replication::registry::ReplicationRegistry`,
  `shared::replication::visibility::ScopeLifetime`;
- from `orrery_persist_client`: `server::uplink::ComponentDiff`,
  `shared::replication::registry::FnsId`, and the `AppMessageExt` surface used
  by `add_message::<ComponentDiff>`.

Because the wrapper is the only declarer, any crate that wants to bypass it
must first add `bevy_replicon` to its own `Cargo.toml`. That is a manifest
change, not a hidden `use` line.

`scripts/core-gates.sh` clause 1 already runs `cargo tree -p <crate>` for every
ruleset-bearing crate (lines 223-227). Extending that loop with a small
denylist/allowlist — e.g. "only `<wrapper-crate>` may have `bevy_replicon` in its
normal dependency tree" — is new policy, but it is not new machinery. The same
per-crate `cargo tree` scan that refuses Bevy inside `orrery_games` can refuse
an unexpected `bevy_replicon` declaration anywhere else.

### 4.3 Three checks before recommending it

**Whole payload-registration surface.** The wrapper can cover every high-level
path in `AppRuleExt` (`replicate`, `replicate_once`, `replicate_diff`,
`replicate_as`, `replicate_with`, `replicate_bundle`, and the filtered/once
variants) by defining its own extension trait with the same names and tighter
bounds. Lower-level paths such as `RuleFns::<C>::default()` or
`ReplicationRegistry::register_rule_fns::<C>` remain reachable only if a crate
can name `bevy_replicon`; the dependency gate closes that. So the surface is
coverable, but the wrapper is a *facade*, not merely a registration helper: it
must also re-export the visibility and message types the two existing replicon
consumers need.

**One wrapper or two.** One wrapper suffices. Both `orrery_spatial` and
`orrery_persist_client` use the same `AppRuleExt` registration surface. The only
difference in feature footprint is that `orrery_persist_client` needs the
`uplink` feature (`bevy_replicon = { workspace = true, features = ["uplink"] }`
at `crates/orrery_persist_client/Cargo.toml:37`). The wrapper can expose an
`uplink` feature that forwards to `bevy_replicon/uplink` and re-exports
`server::uplink::ComponentDiff`. Default features can mirror replicon's own
defaults.

**Derive macros.** `bevy_replicon` defines no proc macros. The derives used in
its own sources and in Orrery consumers (`Component`, `Resource`, `Event`,
`Message`) come from Bevy crates (`bevy_ecs`, `bevy_reflect`, etc.) that are
already declared separately. No replicon derive expands to a
`::bevy_replicon::…` path, so redirecting the crate name through a wrapper does
not break macro expansion.

### 4.4 Honest difference from a fork

A fork makes the bound unbypassable *within the type system*: even a crate with
a direct `bevy_replicon` dependency could not call `replicate::<Bad>()` because
the vendored method's `where` clause would refuse `Bad`. The dependency gate
makes it unbypassable *without a manifest change that a gate refuses*: a crate
that adds `bevy_replicon` can still call the unbounded methods. Those are not
the same guarantee.

But #638's own framing is that "review is currently the only thing there". A
gate that fails a PR adding an unauthorized `bevy_replicon` dependency is
tooling-enforced. Whether that is enough is the owner-reserved OD-26 decision;
the spike's job is to state the option accurately.

---

## 5. What would require a fork

A non-bypassable *type-system* guard would need the bound to appear on the
vendored methods themselves:

- `AppRuleExt::replicate`, `replicate_with`, `replicate_bundle`, and their
  variants would need `C: EngineHandleFree`.
- `IntoComponentRule` for `RuleFns<C>` would need the same bound.
- `RuleFns::<C>::new`/`default` and
  `ReplicationRegistry::register_rule_fns::<C>` would need it.

That is a patch across roughly the files quoted above. The maintenance cost is
not the size of the diff (it is small) but the ongoing vendored-fork burden:

- Every upstream `bevy_replicon` bump must be merged against the changed bounds.
- The vendored crate's own examples, doctests, and tests use Bevy types such as
  `Transform` and `Name`. Those types contain no engine handles, but they do not
  implement `EngineHandleFree`, so they would fail the new bounds unless the
  marker is also implemented for them or the examples are disabled.
- Any first-party code that legitimately replicates a mapped `Entity` through a
  custom `RuleFns` (e.g. `MappedComponent` in replicon's own docs at
  `rules.rs:567-571`) would need an explicit opt-out, which is additional API
  surface to maintain.

The repository already maintains a vendored fork for the uplink change-detection
patch (root `Cargo.toml`), so the infrastructure for a fork exists. The decision
is whether the stronger guarantee is worth that ongoing cost compared with a
wrapper-plus-gate approach.

---

## 6. Compile-time versus registry-time

The answer differs sharply between the two shapes OD-26 is choosing between.

- **Compile-time (`EngineHandleFree` bound):** Replicon's API does not
  *directly* admit the bound, but it does not block a wrapper from enforcing it.
  The wrapper alone is review-enforceable; the wrapper plus a dependency gate is
  tooling-enforceable against manifest changes. Only a fork makes it
  type-system-enforceable. The corrected verdict is therefore "possible without
  a fork; enforceable without a manifest bypass via a dependency gate; fully
  non-bypassable only with a fork."

- **Registry-time schema refusal:** This shape does not interact with replicon's
  registration API at all. It depends on R8's capability/declaration registry
  (A8/#404) and refuses a `(ComponentTypeId, SchemaVersion)` declaration whose
  codec schema contains an engine-handle type. Because it lives at declaration
  time, it reaches persistence and witnessing as well as replication, and it
  requires no change to `vendor/bevy_replicon`.

So the question A9 §9 asked only matters if the owner picks the compile-time
option.

---

## 7. On F-9 / `trybuild`

The eventual F-9 acceptance criterion is a `trybuild` suite with committed
`.stderr` and a positive twin. This spike did **not** wire `trybuild`; running a
full fixture suite would have triggered a multi-crate build while other lanes
are active, which the house rules ask to avoid. The path is unblocked: any crate
that hosts the registration seam can add `trybuild` as a dev-dependency and
`tests/ui/**/*.rs` fixtures. A future implementation PR should add that suite
after OD-26 selects the mechanism.

---

## 8. References

- A9 §9 (`docs/plans/a9-engine-boundaries.md:428-441`): the unevidenced question.
- ADR-0045 IV-7 (`docs/adr/0045-per-component-capability-policy.md:242-243`,
  `:364-379`): the rule and the two priced mechanisms.
- `vendor/bevy_replicon/src/shared/replication/rules.rs:23-28`, `:774-776`,
  `:896-898`: `AppRuleExt` entry points.
- `vendor/bevy_replicon/src/shared/replication/rules/component.rs:60-65`:
  `IntoComponentRule` for `RuleFns<C>`.
- `vendor/bevy_replicon/src/shared/replication/registry/rule_fns.rs:88-95`,
  `:179-186`: `RuleFns` constructors.
- `vendor/bevy_replicon/src/shared/replication/registry.rs:108-119`:
  `register_rule_fns`.
- `crates/orrery_spatial/Cargo.toml:29`: the first `bevy_replicon` declaration.
- `crates/orrery_persist_client/Cargo.toml:37`: the second `bevy_replicon`
  declaration, with the `uplink` feature.
- `crates/orrery_games/Cargo.toml`: confirms `orrery_games` does not declare
  `bevy_replicon`.
- `scripts/core-gates.sh:223-227`: the per-crate `cargo tree` scan that would
  host a `bevy_replicon` declaration gate.
- Rust 2018 extern prelude: verified with a minimal workspace where a consumer
  depending only on a wrapper cannot name the wrapper's transitive dependency.
  This matches the Rust Reference: the extern prelude contains only crates
  named in the current crate's `Cargo.toml`.

(End of file - total 411 lines)
