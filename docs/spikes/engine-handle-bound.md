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

**The bound can be added downstream through a wrapper, but it cannot be made
non-bypassable on the vendored API without a fork.**

Stated more precisely: `bevy_replicon`'s own registration entry points do not
require, and cannot be made to require from outside, a sealed
`EngineHandleFree` marker. A first-party extension trait that forwards to those
entry points can impose the bound at every Orrery-owned call site, and a
component carrying `Entity` (or any other engine handle) then fails to compile
at that wrapper call site. The underlying replicon methods remain directly
callable, so the guard is exactly as strong as the team's willingness to route
registration through the wrapper and to keep direct `replicate`/`replicate_with`
calls out of first-party code.

If the owner wants a guard that cannot be bypassed by simply importing
`bevy_replicon::prelude::*`, the vendored crate must be patched. That is a new
owner decision, not a widening of this spike.

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

## 4. What would require a fork

A non-bypassable guard would need the bound to appear on the vendored methods
themselves:

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
wrapper-plus-policy approach.

---

## 5. Compile-time versus registry-time

The answer differs sharply between the two shapes OD-26 is choosing between.

- **Compile-time (`EngineHandleFree` bound):** Replicon's API does not
  *directly* admit the bound, but it does not block a wrapper from enforcing it.
  The verdict is therefore "possible without a fork, but bypassable without a
  fork; non-bypassable requires a fork."

- **Registry-time schema refusal:** This shape does not interact with replicon's
  registration API at all. It depends on R8's capability/declaration registry
  (A8/#404) and refuses a `(ComponentTypeId, SchemaVersion)` declaration whose
  codec schema contains an engine-handle type. Because it lives at declaration
  time, it reaches persistence and witnessing as well as replication, and it
  requires no change to `vendor/bevy_replicon`.

So the question A9 §9 asked only matters if the owner picks the compile-time
option.

---

## 6. On F-9 / `trybuild`

The eventual F-9 acceptance criterion is a `trybuild` suite with committed
`.stderr` and a positive twin. This spike did **not** wire `trybuild`; running a
full fixture suite would have triggered a multi-crate build while other lanes
are active, which the house rules ask to avoid. The path is unblocked: any crate
that hosts the registration seam can add `trybuild` as a dev-dependency and
`tests/ui/**/*.rs` fixtures. A future implementation PR should add that suite
after OD-26 selects the mechanism.

---

## 7. References

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
