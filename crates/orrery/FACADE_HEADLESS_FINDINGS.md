# Facade headless run (#873)

Run on 2026-09-01 by `tests/facade_headless.rs`, using two independent Bevy
`App`s, two real relay-disabled iroh endpoints, and the synthetic ruleset in
`tests/support/mod.rs`.

## Results

| Question | Result | Classification |
|---|---|---|
| Does `OrreryClientPlugins<Synthetic>` build and run in a `MinimalPlugins` app? | Yes, after the host adds `StatesPlugin`; both apps execute Startup and Update without a panic. | Missing consumer contract, already documented in `Cargo.toml` and the older composition test: lightyear initializes states and `MinimalPlugins` does not install their schedule. `DefaultPlugins` supplies it. |
| Do two real endpoints discover each other? | Yes. One endpoint dials the other's `EndpointAddr` with Orrery's ALPN; both `PeerRegistry` resources converge on the other endpoint's authenticated id. | Working facade path. |
| Does a registered entity replicate and converge? | No. The sender retains `ReplicatedPayload(41)` locally and the receiver never obtains it. | Facade defect. `orrery_net` creates `aeronet_io::Session` entities, while `OrreryPredictPlugin` installs lightyear's client replication backend. No code relates a session to a lightyear `Link`, installs a replication sender for a P2P peer, or carries lightyear/replicon channels over the iroh session. Upstream publishes a `lightyear_aeronet` adapter, but Orrery does not depend on, configure, or attach it. |
| Is prediction exercised by a real reconciliation residual? | No. | Blocked by the facade defect above, not papered over. There is no authoritative replication snapshot from which lightyear can detect a misprediction and produce `VisualCorrection`. Independently, a game must register its predicted component and correction projection and attach Orrery's `PredictedBy`; those are legitimate game-owned consumer contracts. Manually inserting `VisualCorrection` would only re-prove `orrery_predict/tests/reconciliation.rs`, not exercise rollback. |

The passing characterization test
`connected_iroh_sessions_do_not_become_replication_capable_lightyear_links`
is deliberately the inverse assertion. It opens the real links, attempts the
registered payload send, and pins the precise missing boundary. When the
facade grows the adapter, this test must be replaced by state convergence and
a correction caused by a deliberately wrong predicted value.

## Startup contracts observed

The facade documentation already names the other host-owned inputs:
`ContactObservations`, `ContactTick`, `WitnessClock`, and `WitnessIdentity`.
They are not startup blockers. Leaving them unwritten disables the game-owned
contact/witness behavior described by their docs; it does not explain the
missing replication transport.

## Double-edit guards

The facade now refuses startup when `SpatialConfig.high_rate_cap` differs from
`orrery_predict::HIGH_RATE_SET`, or when `PredictConfig.tick_hz` differs from
`orrery_core::TICK_HZ`. Both previously formed silent cross-crate double edits.
The checks live at the composition root, the only freeze-safe place that sees
both sides.

## Mutation check

`OrreryNetPlugin` was temporarily removed from the production
`OrreryClientPlugins` group, then this named test was run:

```text
cargo test -p orrery --test facade_headless \
  two_facades_start_and_discover_over_real_iroh_endpoints -- --nocapture

test two_facades_start_and_discover_over_real_iroh_endpoints ... FAILED
Parameter Res<'_, IslandMembership> failed validation: Resource does not exist
Parameter Res<'_, CoordinatorLink> failed validation: Resource does not exist
```

The mutation was restored. This is the facade member the real endpoint run
depends on; removing it fails the named test before a test-only substitute can
hide the break.
