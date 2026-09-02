# Facade headless run (#873, #889)

Run on 2026-09-02 by `tests/facade_headless.rs`, using two independent Bevy
`App`s, two real relay-disabled iroh endpoints, and the synthetic ruleset in
`tests/support/mod.rs`.

## Results

| Question | Result | Classification |
|---|---|---|
| Does `OrreryClientPlugins<Synthetic>` build and run in a `MinimalPlugins` app? | Yes, after the host adds `StatesPlugin`; both apps execute Startup and Update without a panic. | Missing consumer contract, already documented in `Cargo.toml` and the composition test: Lightyear initializes states and `MinimalPlugins` does not install their schedule. `DefaultPlugins` supplies it. |
| Do two real endpoints discover each other? | Yes. One endpoint dials the other's `EndpointAddr` with Orrery's ALPN; both `PeerRegistry` resources converge on the other endpoint's authenticated id. | Working facade path. |
| Does a registered entity replicate and converge? | Yes. `ReplicatedPayload(41)` crosses the real iroh session, and a later authoritative value of `77` converges on the receiver. | The facade now creates a P2P Lightyear link for each established session and carries its packets through `orrery_net`'s state lane. |
| Is prediction exercised by a real reconciliation residual? | Yes. The receiver deliberately predicts position `9000`; the next replicated authoritative position is `77`. Lightyear records a rollback, produces `VisualCorrection`, and Orrery's monitor observes a non-zero residual for the entity. | This exercises the production rollback and monitor path, but **not game simulation**: the test registers no `FixedUpdate` system and never calls `Synthetic::step`. Lightyear rolls back a test-written Bevy value and re-runs an otherwise empty `FixedMain`. |

Replicon is the intended sender, not a parallel path. D4's stack is Aeronet
transport → Replicon replication backend → Lightyear prediction/replication.
`orrery_replicon` remains the guarded component-registration facade, while
Lightyear drives the pinned Replicon sender and receiver schedules. The bridge
lives in `orrery_predict`, the only first-party Lightyear-facing seam, and
translates its links to `orrery_net::PeerPacket`/`SendPacket`. Using upstream's
generic Aeronet adapter would compete for the same session queues and bypass
Orrery's channel tagging and upload-budget path.

## Game-owned declarations

The facade supplies the replication sender and transport bridge. A game still
declares the pieces that depend on its types and gameplay: replicated component
schemas, interpolation/correction policy, and replication/prediction targets on
entities. The fixture does all three for `PredictedPosition`; they are also
called out on the `game → facade` edge in `docs/10-crates.md`.

The other documented host-owned inputs remain `ContactObservations`,
`ContactTick`, `WitnessClock`, and `WitnessIdentity`. Leaving them unwritten
disables the game-owned contact/witness behavior described by their docs; it
does not disable replication.

## Double-edit guards

The facade refuses startup when `SpatialConfig.high_rate_cap` differs from
`orrery_predict::HIGH_RATE_SET`, or when `PredictConfig.tick_hz` differs from
`orrery_core::TICK_HZ`. Both previously formed silent cross-crate double edits.
The checks live at the composition root, the only freeze-safe place that sees
both sides.

## Mutation check

The production replication bridge was temporarily removed from
`OrreryClientPlugins` without changing the fixture. The exact named test still
compiled, ran alone, and failed on the first state convergence assertion:

```text
cargo test -p orrery --test facade_headless \
  two_facades_start_and_discover_over_real_iroh_endpoints -- --exact --nocapture

running 1 test
thread 'two_facades_start_and_discover_over_real_iroh_endpoints' panicked at crates/orrery/tests/facade_headless.rs:108:9:
timed out for the registered entity state to converge through the facade bridge
test two_facades_start_and_discover_over_real_iroh_endpoints ... FAILED

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 10.11s
```

The bridge was restored after the mutation.

# Synthetic-rules sidecar (#898 step 1)

`examples/synthetic_sidecar.rs` promotes the same headless composition to a
runnable `main`: `MinimalPlugins + StatesPlugin +
OrreryClientPlugins<Synthetic>`. It registers one predicted component and a
game-owned `FixedUpdate` adapter that constructs the rules view and RNG, calls
`Synthetic::step`, mirrors the result into `PredictedPosition`, and writes the
tick's `PoseSample`.

The named proof records a rules-produced anchor at tick 6, creates a wrong
local future by writing only `9000`, and lets the rules produce `9001..9003`.
It then deposits the captured anchor into Lightyear's history and requests the
real state rollback. Lightyear reports one rollback, re-runs `FixedMain` for
ticks 7–9, the step trace observes rules-produced `7, 8, 9`, and
`PredictionHistory` retains `9` at the present tick. The latest `PoseSample`
also contains `9`.

One Lightyear detail matters when reading the assertion: after `App::update`,
the live registered component is deliberately the frame-interpolated
presentation sample. The fixed simulation value is
`PredictionHistory[current_tick]`; Lightyear restores that value before the
next fixed tick. Asserting the live post-render sample equals the fixed value
would reject Lightyear's intended interpolation, not catch a rollback fault.

## Platform gaps and game-owned work exposed

- ~~`orrery_authority` does **not** populate `PredictedBy`; its only
  first-party writers are fixtures.~~ **Closed by #910.** The gap was real —
  and so was the reason for it: `orrery_authority` sits *below* `orrery_predict`
  on the spine and cannot name the component without inverting it. The writer
  therefore landed at the composition root, as
  `orrery::track_predicted_authority` in `OrreryAuthorityAttributionPlugin`,
  stamping the settled `Authority.holder` onto every entity carrying a
  `PersistIdentity`. A game adapter no longer joins predicted entities to their
  authority by hand; it still owns the `CoreState` ↔ predicted-component
  adapter below.
- `orrery_authority` does expose both `PoseSample` and
  `PoseHistory::record`; no frozen-crate reach-through is needed. There is no
  production writer, though. Step 1 projects the latest pose; step 4 must add
  the per-tick `PoseHistory::record` call on the authority side.
- The facade cannot supply the `CoreState` ↔ predicted-component adapter, the
  predicted fixed-step system, simulation membership, ordered game inputs,
  emitted-event handling, or the game's hit radius. Those depend on game
  types or semantics.
- Under `MinimalPlugins`, the host must add `StatesPlugin`. A normal
  `DefaultPlugins` client already has it.
- The previously documented game inputs remain game-owned: replicated schema,
  interpolation and correction policy, replication and prediction targets,
  `ContactObservations`, `ContactTick::tick`, `WitnessClock`, and
  `WitnessIdentity`.

The current #898 body retrieved by `gh issue view 898` contains no numbered
twelve-item game-supply list. Counting the bullets above as a fixed number
would be misleading because several are deliberately compound (for example,
component registration versus its interpolation and correction policies).

## Step-1 mutation check

Removing only
`app.add_systems(FixedUpdate, step_synthetic_rules)` left the exact test
compiling and produced this named failure:

```text
cargo test -p orrery --example synthetic_sidecar \
  tests::rollback_reexecutes_synthetic_step_and_keeps_its_rules_produced_value \
  -- --exact --nocapture

running 1 test
thread 'tests::rollback_reexecutes_synthetic_step_and_keeps_its_rules_produced_value' panicked at crates/orrery/examples/synthetic_sidecar.rs:257:9:
assertion `left == right` failed: the rollback anchor must itself be rules-produced
  left: PredictedPosition(0)
 right: PredictedPosition(6)
test tests::rollback_reexecutes_synthetic_step_and_keeps_its_rules_produced_value ... FAILED

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out
```

The registration was restored after the mutation.
