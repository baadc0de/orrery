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
| Is prediction exercised by a real reconciliation residual? | Yes. The receiver deliberately predicts position `9000`; the next replicated authoritative position is `77`. Lightyear records a rollback, produces `VisualCorrection`, and Orrery's monitor observes a non-zero residual for the entity. | This exercises the production rollback and monitor path; the test never inserts `VisualCorrection` itself. |

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
