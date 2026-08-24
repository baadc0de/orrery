<!-- Committed verbatim from the owner-supplied brief of 2026-08-24 so that
     every agent working #395 reads the same source of truth. Do not edit the
     body below to reflect later decisions: decisions live in the #395 tree and
     in ADRs, and a brief that quietly tracks them stops being evidence of what
     was originally proposed. -->

> **Repository note.** This is the source document for epic **#395**. It is a
> *proposal for critique*, not an approved design, and nothing in it is
> normative. Where it conflicts with an Accepted ADR, the ADR wins — see
> `AGENTS.md`. Decisions it asks for are recorded in the #395 planning tree.

# Orrery: Migrate `Ruleset` Toward a Bevy ECS Simulation Architecture

**Document type:** architecture critique and planning brief  
**Audience:** coding agent, technical lead, or architecture reviewer  
**Status:** proposal for critique; not an approved design  
**Date:** 2026-08-24

## Assignment for the coding agent

Inspect the current Orrery repository and use this document as a hypothesis to critique and groom into an implementable architectural plan.

Do **not** accept the proposed architecture uncritically. Verify every claim against the current code, Cargo feature graph, tests, ADRs, wire protocol, persistence model, prediction stack, witnessing model, and service boundaries.

Your output should:

1. Describe the current `Ruleset` abstraction and every crate/API it affects.
2. Identify which problems in this proposal are real today versus speculative scaling concerns.
3. Compare at least three viable architectural variants.
4. Recommend a target architecture and explain why it is preferable.
5. Produce a phased migration plan with small, reviewable pull requests.
6. Preserve currently passing gates and avoid a flag-day rewrite.
7. List unresolved decisions that require maintainer input.
8. Propose ADR changes or additions.
9. Identify benchmarks, fixtures, conformance tests, and migration tests needed before implementation.
10. Call out any proposal here that conflicts with existing Orrery invariants.

Unless explicitly authorized, do not implement the migration. The first deliverable is an evidence-backed architectural plan.

---

## Executive summary

The existing idea of a game-defined `Ruleset` is valuable because Orrery needs a portable definition of canonical universe behaviour. It becomes dangerous if a complete game is expected to live inside one trait implementation or if a generic `R: Ruleset` propagates throughout the entire crate graph.

The working hypothesis is:

> Retain a single game/universe definition as the composition root, but implement canonical game logic as modular systems and components in a dedicated `bevy_ecs::World`.

Under this model:

- Orrery provides the canonical simulation kernel and its invariants.
- A game assembles multiple rule modules.
- Each module registers components, resources, commands, systems, persistence policies, rollback policies, witness projections, and replication policies.
- Bevy and Unreal are presentation/input integrations around the same canonical Rust simulation.
- No Bevy rendering, windowing, asset, input, or platform APIs enter the canonical simulation.
- `bevy_ecs` is an internal implementation substrate, not part of Orrery's network protocol or foreign-function interface.

This proposal deliberately does **not** claim that ECS automatically solves determinism, persistence, rollback, versioning, authority, or cross-module corner cases. Orrery must define those guarantees above the ECS.

---

## Motivation

### What is good about `Ruleset`

A portable rules abstraction potentially gives Orrery:

- One canonical implementation of shared universe behaviour.
- The same rules under a Bevy client, headless process, service, replay tool, or Unreal bridge.
- A clear boundary between reusable infrastructure and game-specific semantics.
- Versioned rules suitable for persistence, witnessing, replay, and compatibility checks.
- An explicit place to define deterministic simulation constraints.

### What becomes dangerous

At AAA scale, a monolithic rules implementation risks becoming responsible for:

- Movement and navigation.
- Ships, stations, characters, and projectiles.
- Damage and repair.
- Inventory and equipment.
- Economy and transactions.
- Construction and destruction.
- Docking and attachment.
- AI and mission logic.
- Authority-sensitive interactions.
- Persistence and migrations.
- Prediction and rollback behaviour.
- Cross-cutting combinations of all of the above.

The likely failure modes are:

- A god trait or god implementation.
- Generic type parameters infecting unrelated crates.
- Large conditional branches for game modes and optional features.
- Unclear system ordering.
- Hidden coupling through callbacks.
- Corner cases accumulating in central dispatch functions.
- Long compile times and excessive monomorphization.
- Difficult unit testing and poor ownership boundaries.
- A foreign-function boundary that exposes Rust/Bevy implementation details.
- Pressure to duplicate canonical logic in Unreal C++ or Blueprint.

The goal is not to eliminate a game-wide definition. It is to change it from a container of behaviour into a composition root and compatibility manifest.

---

## Current-state assumptions to verify

These are working assumptions from the current architectural discussion. The coding agent must verify and correct them.

1. `orrery_protocol` and `orrery_core` are intentionally free of full-engine Bevy dependencies.
2. `Ruleset` currently lives in, or is primarily owned by, the engine-independent core.
3. The built client path composes Orrery networking, spatial, authority, prediction, witnessing, and persistence-client plugins.
4. Aeronet, Replicon, and Lightyear are currently important implementation dependencies of the Bevy client path.
5. Prediction is the area most tightly coupled to Lightyear/Bevy semantics.
6. Orrery uses stable persistent/network identity separately from ephemeral Bevy entities.
7. Orrery requires fixed-step simulation, scoped determinism, rollback, authority handoff, persistence, and witnessing.
8. The recently completed external bridge demonstrates a directional framed boundary but does not yet define a complete engine-neutral playable-client API.
9. Services and wire types should not need access to rendering or presentation worlds.
10. The intended future Unreal integration should consume commands and presentation frames rather than reimplement Orrery using Actor replication or Iris.

For each assumption, cite the relevant source file, test, ADR, or issue.

---

## Proposed conceptual model

### 1. Orrery simulation kernel

The kernel owns universal invariants rather than game-specific content:

- Simulation time and tick progression.
- Stable identity.
- Spatial cells and canonical coordinates.
- Command admission and ordering.
- Authority leases and handoff rules.
- Transaction boundaries.
- Persistence coordination.
- Rollback history and correction boundaries.
- Witness construction and verification.
- Module/schema manifests.
- Deterministic scheduling constraints.

It may use `bevy_ecs` internally, but it must not make `bevy_ecs::Entity`, `World`, `ComponentId`, events, or reflected type information part of the wire protocol.

### 2. Rules modules

A rules module owns a coherent gameplay domain. Illustrative modules:

- `orrery_rules_spatial`
- `orrery_rules_ship`
- `orrery_rules_damage`
- `orrery_rules_inventory`
- `orrery_rules_docking`
- `orrery_rules_economy`
- `orrery_rules_construction`
- `orrery_rules_ai`

These names are examples, not proposed crate names.

A module may register:

- ECS components and resources.
- Command and event types.
- Systems in explicit deterministic stages.
- Schema IDs and versions.
- Persistence encoding and migrations.
- Rollback inclusion and restore behaviour.
- Witness/hash projection.
- Replication and relevance policy.
- Authority requirements.
- Dependencies and incompatibilities.
- Validation and conformance tests.

### 3. Integration modules

Modules cannot be assumed to be semantically independent. Cross-domain behaviour should be explicit.

Examples:

- Hull damage affecting cargo.
- Docking transferring or constraining authority.
- Inventory ownership interacting with economy transactions.
- Construction changing spatial topology.
- Destruction emitting persistence tombstones.

Rather than placing these rules arbitrarily in either parent module, introduce explicit integration modules such as:

- `ShipCargoDamageIntegration`
- `DockingAuthorityIntegration`
- `EconomyInventoryIntegration`

The planning agent should evaluate whether these are modules, systems registered by a higher-level game crate, or another construct. The important requirement is that cross-module coupling be visible, ordered, testable, and owned.

### 4. Game definition / assembled ruleset

The complete game retains one identity and one assembled manifest, but not one giant implementation body.

Illustrative API only:

```rust
pub trait GameDefinition {
    fn game_id(&self) -> GameId;
    fn protocol_version(&self) -> ProtocolVersion;
    fn install(&self, builder: &mut SimulationBuilder);
}

impl GameDefinition for ExampleGame {
    fn game_id(&self) -> GameId {
        GameId::new("example-game")
    }

    fn protocol_version(&self) -> ProtocolVersion {
        ProtocolVersion::new(1)
    }

    fn install(&self, builder: &mut SimulationBuilder) {
        builder.add_module(CoreSpatialModule);
        builder.add_module(ShipModule);
        builder.add_module(DamageModule);
        builder.add_module(InventoryModule);
        builder.add_module(DockingModule);
        builder.add_module(ShipCargoDamageIntegration);
    }
}
```

This API should not be copied directly. Determine whether a trait, builder, plugin group, static function, declarative macro, inventory registration, or generated manifest best fits the current code.

---

## Proposed ECS boundary

### Dedicated canonical world

The preferred hypothesis is a dedicated canonical `bevy_ecs::World`, separate from presentation state even when Orrery is hosted by the Bevy engine.

```text
Canonical simulation world          Engine presentation world
--------------------------          -------------------------
PersistId                           Render entity / Actor
CanonicalPosition                  Transform / FVector
Velocity                           Animation parameters
AuthorityLease                     Selection and UI state
HullIntegrity                      Damage effects
Inventory                          Meshes and materials
Transaction state                  Audio and particles
```

The canonical world should advance at Orrery's fixed simulation rate. The presentation engine may advance at an unrelated frame rate and consume interpolated presentation frames.

Benefits to verify:

- Presentation components cannot accidentally enter witness or persistence state.
- Rollback does not rewind audio, particles, UI, or engine-local objects.
- The identical simulation world can run headlessly, under Bevy, or behind Unreal.
- Simulation timing is independent from renderer timing.
- An explicit output contract simplifies testing and FFI.
- Only entities in the local presentation/AOI set need to be mirrored.

Costs to measure:

- State extraction and copying.
- Duplicate entity lookup maps.
- Latency between canonical and presentation worlds.
- Added debugging complexity.
- Difficulty reusing Bevy-native plugins expecting the main application world.
- Lightyear/Replicon integration complications if they expect direct access to presentation entities.

The agent must compare this with a single-world Bevy design and an engine-neutral non-ECS core.

### Stable identity

`bevy_ecs::Entity` must remain runtime-local and ephemeral. Network, persistence, replay, authority, and FFI references must use Orrery-owned stable IDs.

Illustrative component:

```rust
#[derive(Component)]
pub struct PersistId(pub StableEntityId);
```

The plan must establish:

- Creation and allocation rules.
- Bidirectional `StableEntityId <-> Entity` lookup ownership.
- Despawn and tombstone semantics.
- Rollback behaviour for identity maps.
- Cross-island and cross-cell references.
- How stale references are detected.
- Whether transient predicted entities receive a distinct identity class.

---

## Deterministic scheduling model

Bevy ECS scheduling does not automatically provide Orrery-level determinism. The target needs a constrained canonical schedule.

Illustrative stages:

```text
ReceiveCommands
ValidateCommands
ApplyAuthoritativeInputs
PreSimulate
Simulate
ResolveInteractions
CommitTransactions
FinalizeAuthority
RecordRollback
BuildWitness
EmitPresentation
```

The exact stages must be derived from current Orrery behaviour. Do not invent ordering that contradicts Lightyear prediction, persistence commit semantics, or authority transitions.

Canonical systems should be prohibited or constrained from using:

- Wall-clock time.
- Unseeded or shared implicit randomness.
- Platform-dependent APIs.
- Arbitrary asynchronous mutation.
- Unordered collections where order affects observable results.
- Presentation-engine state.
- Process-global mutable state.
- Nondeterministic I/O.

Questions the plan must answer:

1. Is deterministic execution required across processes, architectures, compiler versions, or only within a narrower compatibility envelope?
2. Can independent systems execute in parallel?
3. How are events ordered when multiple producers run concurrently?
4. Are deferred ECS commands permitted in canonical stages?
5. How are structural changes ordered?
6. Is query iteration order ever semantically observable?
7. What floating-point policy applies?
8. How is randomness partitioned by system, entity, authority island, and tick?
9. How is schedule topology incorporated into the compatibility hash?
10. How are determinism violations detected in tests and debug builds?

---

## Component policy registry

ECS component registration alone is insufficient. Orrery needs explicit policies for canonical behaviour.

Illustrative API:

```rust
builder.register_component::<CargoHold>(ComponentPolicy {
    schema: SchemaId::new("cargo-hold", 3),
    persistence: PersistencePolicy::Persistent,
    rollback: RollbackPolicy::Included,
    witness: WitnessPolicy::Included,
    replication: ReplicationPolicy::Relevant,
    authority: AuthorityPolicy::EntityAuthority,
});
```

The real design may use separate registries or traits. The plan must avoid a single overgeneralized policy object if the domains have different requirements.

At minimum, address:

- Stable schema IDs independent of Rust type names.
- Versioned codecs.
- Forward and backward compatibility.
- Data migrations.
- Defaulting and removal.
- Unknown component handling.
- Persistence inclusion.
- Rollback inclusion.
- Witness inclusion and canonical encoding.
- Replication direction and relevance.
- Authority required to mutate.
- Privacy/security filtering.
- Maximum encoded size and denial-of-service limits.

Reflection may assist tooling but should not silently define the persistence or wire format.

---

## Commands, events, and queries

The architecture should distinguish these concepts clearly:

- **External command:** a request entering the canonical simulation from a peer, service, engine, or replay.
- **Internal command:** a deterministic request scheduled for application within the simulation.
- **Domain event:** a canonical result that other rule modules may consume.
- **Persistence event:** a durable change or transaction result.
- **Presentation event:** an engine-facing consequence with no authority over canonical state.
- **Diagnostic event:** tracing, metrics, warnings, or conformance output.

Avoid unbounded observer chains where execution ordering is implicit. Establish whether events are immediate, buffered, stage-delimited, or tick-delimited.

The plan should specify:

- Ordering rules.
- Delivery guarantees.
- Replay behaviour.
- Rollback behaviour.
- Deduplication/idempotency.
- Maximum event volume.
- Whether events are stored or derived.
- How integration modules subscribe without creating cyclic dependencies.

---

## Persistence, rollback, and witnessing

These are not generic ECS serialization problems.

### Persistence

Determine whether Orrery persists:

- Complete canonical snapshots.
- Component-level changes.
- Domain events.
- Transaction journals.
- A hybrid of snapshots and journals.

The module system must not let modules bypass transactional persistence invariants.

### Rollback

Determine the rollback unit:

- Entire simulation world.
- Authority island.
- Spatial cell.
- Entity set.
- Component subset.

Raw cloning or serialization of the complete `World` is unlikely to be acceptable without evidence. The plan should compare component journals, archetype snapshots, copy-on-write approaches, and Lightyear's existing history mechanisms.

### Witnessing

Witness hashes require a canonical projection and encoding. Bevy archetype order, component insertion order, entity allocation order, reflection metadata, and hash-map order must not affect the result.

The manifest should identify:

- Witnessed schemas and versions.
- Canonical entity ordering.
- Canonical component ordering.
- Canonical byte encoding.
- Tick and authority context.
- Excluded transient/cosmetic data.

---

## Bevy integration

Evaluate two primary variants.

### Variant A: separate canonical world

- Orrery owns a dedicated `bevy_ecs::World` and schedules.
- The Bevy engine adapter submits inputs and consumes presentation frames.
- The same core can run in a sidecar or embedded Unreal bridge.

### Variant B: shared Bevy application world

- Canonical and presentation components coexist.
- Schedules and component policy registries define the boundary.
- Less mirroring and potentially easier Lightyear integration.
- Greater risk of accidental coupling, rollback contamination, and Unreal divergence.

The agent should assess whether Bevy `SubApp`, manually owned `World`, or another mechanism best supports Variant A in the current Bevy version.

---

## Unreal integration implications

The proposed ECS substrate is compatible with Unreal if it remains behind a narrow runtime interface.

Illustrative boundary:

```text
Unreal -> Orrery
- input/interaction commands
- view and interest hints
- session requests

Orrery -> Unreal
- spawn/despawn batches
- interpolated/predicted presentation transforms
- domain and presentation events
- authority changes
- persistence outcomes
- corrections and rollback notices
```

No `bevy_ecs` type should cross the C ABI or local IPC boundary.

The bridge should use:

- Opaque runtime handles.
- Fixed-width integers.
- Explicit byte buffers and ownership.
- Stable entity and schema IDs.
- Batched commands and frames.
- Separate runtime ABI and rules-manifest versions.

The agent should preserve the possibility of both:

1. A headless Rust/Bevy sidecar used by an Unreal plugin.
2. An embedded Rust static/dynamic library owning the same canonical ECS world.

Do not make Unreal Actor replication, Iris, Mass, Chaos, UObject identity, or Blueprint state canonical Orrery truth. Those may mirror or consume canonical state.

---

## Architectural variants to compare

The critique must compare at least these variants.

### Variant 1: retain the current `Ruleset`

Improve it incrementally with subtraits, delegation, and helper crates.

Evaluate:

- Lowest migration risk.
- Whether current pain actually justifies a redesign.
- Continued generic propagation.
- Testing and modularity limits.
- Unreal/runtime extraction consequences.

### Variant 2: `bevy_ecs` canonical simulation

Use a dedicated ECS world with rule modules and Orrery-owned registries.

Evaluate:

- Alignment with the existing Bevy/Lightyear stack.
- Scheduling and modularity.
- Determinism controls.
- Persistence/rollback implementation.
- Compile times and Bevy upgrade churn.
- Headless and Unreal embedding.

### Variant 3: engine-neutral bespoke simulation core

Use normal Rust data structures and explicit dispatch, with Bevy and Unreal adapters.

Evaluate:

- Strongest nominal engine independence.
- Cost of rebuilding ECS scheduling, queries, storage, and tooling.
- Easier or harder determinism.
- Potentially simpler serialization.
- Integration friction with Lightyear and Bevy.

### Optional Variant 4: hybrid

Keep a small deterministic transaction/rules core outside ECS while representing world state and most behaviours in `bevy_ecs`.

Evaluate whether this produces a clean separation or merely two competing models.

Provide a decision matrix with weighted criteria. Suggested criteria:

- Migration risk.
- Determinism confidence.
- Persistence fit.
- Rollback fit.
- Witnessing fit.
- Modularity.
- Performance.
- Compile-time impact.
- Bevy integration.
- Unreal integration.
- API stability.
- Testability.
- Contributor ergonomics.
- Long-term maintenance.

---

## Compatibility and versioning

An assembled game should produce a canonical manifest. Illustrative contents:

```text
game_id
kernel_version
protocol_version
module_id -> module_version
component_schema_id -> schema_version
command_schema_id -> schema_version
schedule/topology hash
canonical configuration hash
determinism profile
```

The plan must determine:

- What must match between peers.
- What must match between a client and authority holder.
- What may differ cosmetically.
- How rolling upgrades work.
- How persisted data records its producing manifest.
- How replays select the correct implementation.
- How module removal and replacement work.
- Whether runtime-loaded code is in scope.

Initial recommendation: prefer statically compiled module composition with runtime data/configuration. Dynamic native gameplay modules create ABI, security, migration, and deterministic compatibility problems and should require a separate proposal.

---

## Migration principles

1. No flag-day rewrite.
2. Preserve all currently passing P0-P4 gates unless the repository shows a newer status.
3. Introduce seams before moving behaviour.
4. Keep wire compatibility unless a separately approved protocol change is necessary.
5. Keep stable identity independent of ECS entity allocation.
6. Do not change persistence format accidentally.
7. Do not rewrite prediction and rollback simultaneously with module composition.
8. Add conformance tests before moving canonical behaviour.
9. Maintain a compatibility adapter for the existing `Ruleset` during migration where practical.
10. Prefer small PRs with independently valuable outcomes.

---

## Candidate phased migration

This sequence is intentionally provisional. The coding agent should reorder or reject it based on repository evidence.

### Phase 0: repository archaeology and dependency map

Deliverables:

- Call graph and crate dependency map around `Ruleset`.
- Inventory of all associated types, methods, generics, tests, and examples.
- Current lifecycle diagram for commands, prediction, persistence, and witnessing.
- List of current invariants and compatibility gates.
- Baseline build times, binary sizes, and representative simulation benchmarks.

Exit criteria:

- Maintainers agree on the current-state description.
- No code migration has begun.

### Phase 1: establish behavioural conformance fixtures

Deliverables:

- Golden command -> state/event traces for representative rules behaviour.
- Deterministic replay tests across repeated runs.
- Stable persistence and witness fixtures.
- Authority handoff and rollback scenarios.
- Performance baseline.

Exit criteria:

- Existing `Ruleset` behaviour can be compared mechanically with a replacement.

### Phase 2: introduce composition without ECS migration

Possible deliverables:

- `GameDefinition`/composition-root facade.
- Multiple delegated rule modules behind the existing `Ruleset` contract.
- Dependency and manifest validation.
- No externally visible behaviour change.

Purpose:

- Test whether composition alone solves most of the modularity problem.
- Separate the module-model decision from the ECS-storage decision.

Exit criteria:

- At least two existing behaviours are owned by separate modules.
- Existing gates and conformance fixtures pass unchanged.

### Phase 3: introduce the canonical simulation host

Possible deliverables:

- `SimulationHost` owning time, stable IDs, schedule execution, and output collection.
- A narrow command-in/event-out API.
- Existing `Ruleset` hosted through an adapter.
- No requirement yet to store canonical state in ECS.

Exit criteria:

- Bevy client and headless tests invoke the same host API.
- Host lifetime and fixed-step semantics are explicit.

### Phase 4: introduce `bevy_ecs` behind the host

Possible deliverables:

- Dedicated canonical `World`.
- Explicit schedule stages.
- Stable-ID lookup resource.
- Component policy registry skeleton.
- A small vertical slice migrated to ECS.

Choose a low-risk slice that still exercises commands, state mutation, persistence, rollback, and witness projection. Avoid beginning with the hardest Lightyear prediction path.

Exit criteria:

- Old and new implementations pass the same conformance fixture.
- Deterministic replay remains stable.
- Performance regression is understood and accepted.

### Phase 5: migrate domain modules incrementally

For each module:

1. Define owned components and schemas.
2. Define input commands and domain events.
3. Define explicit schedule placement.
4. Register persistence, rollback, witness, replication, and authority policies.
5. Port behaviour.
6. Run differential tests against the legacy implementation.
7. Remove the legacy path only after parity.

Exit criteria per module:

- Behavioural parity or an explicitly approved semantic change.
- Migration fixture for persisted state.
- Rollback and witness coverage.
- No undocumented cross-module dependency.

### Phase 6: isolate canonical and presentation worlds

Possible deliverables:

- Presentation-frame schema.
- Bevy presentation mirror.
- AOI-limited extraction.
- Interpolation contract.
- Diagnostics for mapping and stale entities.

This may occur earlier if repository constraints require it.

Exit criteria:

- Canonical simulation runs without rendering plugins.
- A Bevy presentation client consumes only the public output contract for migrated features.

### Phase 7: remove generic and compatibility debt

Possible deliverables:

- Reduce or eliminate workspace-wide `R: Ruleset` propagation.
- Remove transitional adapters.
- Freeze supported module/manifest APIs.
- Update ADRs and architecture documents.
- Publish example rule modules.

Exit criteria:

- No obsolete dual implementation remains.
- Public extension points are documented and tested.

### Phase 8: Unreal-facing proof

Possible deliverables:

- Headless sidecar using the canonical simulation.
- Minimal C ABI or local IPC adapter.
- Unreal observer that maps stable IDs to Actors.
- Command submission and presentation-frame consumption.

This validates that `bevy_ecs` is internal rather than an engine-facing dependency.

---

## Testing strategy

The groomed plan should define concrete tests in each category.

### Determinism

- Repeat identical command logs many times and compare canonical hashes.
- Vary worker-thread counts.
- Run debug and release profiles.
- Where supported, run multiple target architectures or operating systems.
- Randomize insertion/allocation order while preserving semantic inputs.
- Detect use of unordered collections in witnessed paths.

### Differential behaviour

- Feed identical inputs to legacy and ECS implementations.
- Compare canonical state projections, events, persistence output, and witness hashes.
- Explicitly classify expected differences.

### Persistence

- Load old-format state into the new runtime.
- Save/reload round trips.
- Schema upgrades and downgrades where supported.
- Unknown module/component handling.
- Kill/restart durability gates.

### Rollback and prediction

- Late input correction.
- Entity creation/destruction within rollback windows.
- Cross-module events during rollback.
- Authority handoff inside or adjacent to rollback.
- Presentation event suppression or reconciliation.

### Modularity

- Missing dependency detection.
- Cyclic dependency detection.
- Incompatible module versions.
- Duplicate schema IDs.
- Schedule ambiguity or illegal stage registration.
- Module removal with persisted data present.

### Performance

- Fixed-tick duration by entity count and module set.
- Structural-change cost.
- Snapshot/journal cost.
- Witness construction cost.
- Presentation extraction cost.
- Memory per canonical entity.
- Startup and module-registration cost.
- Incremental compile time and clean build time.

---

## Primary risks

### Bevy upgrade churn

Depending directly on `bevy_ecs` introduces version coupling even without renderer dependencies.

Mitigations to evaluate:

- A narrow Orrery simulation facade.
- Pinning Bevy versions per Orrery release.
- Minimizing direct `bevy_ecs` usage in public APIs.
- Upgrade conformance suites.
- Avoiding dependence on experimental ECS features in canonical paths.

### False confidence in determinism

Passing repeated local tests does not prove cross-platform or cross-version determinism.

Mitigations:

- Define the actual determinism envelope.
- Canonical codecs and ordering.
- Worker-count tests.
- Witness fixtures.
- Static/lint-like restrictions where possible.

### Duplication with Lightyear

Orrery must not unintentionally rebuild prediction, rollback, replication, or interpolation already provided by Lightyear.

The plan must specify which system owns each responsibility before changing code.

### Two-world overhead

Separating canonical and presentation worlds may add latency, copying, mapping, and debugging cost.

Mitigations:

- AOI-limited presentation extraction.
- Batched structure-of-arrays frames where beneficial.
- Stable lookup caches.
- Benchmarks before optimization.

### Overgeneralized module API

A universal plugin interface may become another god abstraction.

Mitigations:

- Prefer separate capability registries.
- Keep the minimum common module lifecycle.
- Allow ordinary Rust crates and functions to remain the primary composition mechanism.
- Add abstractions only for current use cases.

### Premature dynamic extensibility

Runtime-loaded native gameplay code complicates ABI stability, deterministic compatibility, security, migration, and debugging.

Initial recommendation: static composition first.

### Protocol leakage

Using ECS internally must not make component type IDs, archetype order, Bevy entity bits, reflection names, or schedule internals part of durable or network formats.

---

## Questions requiring explicit decisions

The coding agent should answer where evidence permits and flag the remainder.

1. Is `Ruleset` currently solving a real extensibility problem or mainly acting as a test seam?
2. Which exact behaviours are expected to be game-defined?
3. Which behaviours must remain in the Orrery kernel?
4. Does canonical game state already live in a Bevy world?
5. Can the current Lightyear integration run against a dedicated canonical world?
6. What is the required determinism envelope?
7. What is the rollback unit?
8. Which component categories are persisted, replicated, rolled back, and witnessed?
9. Can these policies differ independently?
10. How are schema IDs allocated and governed?
11. How are system ordering and module dependencies validated?
12. Should module composition be compile-time only?
13. Must games be able to add modules without recompiling Orrery itself?
14. How will rules and manifests be versioned in persisted universes?
15. How will old replays select compatible rules?
16. Does the Bevy client share the canonical world or mirror it?
17. What must an Unreal client be allowed to predict locally?
18. Which presentation events must be reversible after rollback?
19. How are services kept independent from ECS implementation details?
20. What is the smallest vertical slice that tests the entire architecture?

---

## Expected architecture-review deliverable

Produce a Markdown report with these sections:

1. **Repository evidence**
   - Current crate graph.
   - Current `Ruleset` surface and call sites.
   - Current dataflow diagrams.
   - Existing tests and gates.

2. **Problem validation**
   - Problems that exist now.
   - Problems likely at larger game scale.
   - Concerns not supported by evidence.

3. **Architecture variants**
   - At least three variants.
   - Weighted decision matrix.
   - Consequences and failure modes.

4. **Recommendation**
   - Target component model.
   - Scheduling model.
   - Module composition model.
   - Persistence/rollback/witness model.
   - Bevy and Unreal boundaries.

5. **Migration plan**
   - Ordered, small PRs.
   - Dependencies between PRs.
   - Compatibility strategy.
   - Rollback plan for each phase.

6. **Verification plan**
   - Tests, fixtures, benchmarks, and acceptance thresholds.

7. **ADR plan**
   - ADRs to create, replace, or amend.

8. **Open decisions**
   - Questions requiring maintainer judgment.

9. **First recommended PR**
   - Exact scope.
   - Files likely affected.
   - Acceptance criteria.
   - Explicit non-goals.

---

## Suggested agent prompt

The following can be supplied directly to a coding agent together with this document:

> Inspect the current Orrery repository and critique `orrery-migrate-ruleset-to-bevy-ecs-guide.md`. Treat it as a hypothesis, not an instruction. Trace the real `Ruleset` API and dataflow through every relevant crate, test, ADR, example, and gate. Identify incorrect assumptions and missing constraints. Compare retaining the current abstraction, migrating canonical simulation to a dedicated `bevy_ecs::World`, using a bespoke engine-neutral core, and any superior hybrid you discover. Produce an evidence-backed architecture recommendation and a sequence of small migration PRs that preserve current behaviour and wire/persistence compatibility. Do not implement the migration. Conclude with the smallest useful first PR and its testable acceptance criteria.

---

## Initial recommendation, subject to critique

The current preferred direction is:

1. Keep a game-wide definition as a composition root and manifest.
2. Replace monolithic behaviour with statically composed rule modules.
3. Use `bevy_ecs` as the internal canonical simulation substrate.
4. Run canonical state in a dedicated world unless measurements or Lightyear constraints strongly favour a shared world.
5. Keep stable Orrery IDs, schemas, codecs, manifests, and policies above ECS.
6. Make deterministic stages and restrictions explicit.
7. Treat persistence, rollback, witnessing, replication, and authority as independently registered capabilities.
8. Expose only commands, events, and presentation frames to Bevy/Unreal adapters.
9. Establish conformance fixtures before migrating behaviour.
10. Avoid dynamic native modules and Unreal-native canonical logic initially.

The critical principle is:

> A complete game may have one assembled ruleset, but it must not require one monolithic rules implementation.

---

## Reference material

- Orrery repository: <https://github.com/baadc0de/orrery>
- Orrery crate architecture: <https://github.com/baadc0de/orrery/blob/main/docs/10-crates.md>
- Orrery Bevy netcode ADR: <https://github.com/baadc0de/orrery/blob/main/docs/adr/0004-bevy-netcode-stack.md>
- Bevy 0.19 release: <https://bevy.org/news/bevy-0-19/>
- Bevy setup and dependency model: <https://bevy.org/learn/quick-start/getting-started/setup/>

