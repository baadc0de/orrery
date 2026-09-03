# #862 box 2 item 3 spike — persistd's gateway consumer

The working artifact behind
[`../862-gateway-consumer-dependency-cycle.md`](../862-gateway-consumer-dependency-cycle.md).

It proves two things against a real FoundationDB cluster and a real compiler:

1. the dependency cycle blocking the obvious wiring is **real, and is a cargo
   package-level error rather than a style rule** — see below;
2. a gateway-side standing-invalidation feed can be written with **no new edge
   on the spine at all**, by reading identity's durable `dc` rows through
   `orrery_persistd` alone.

`main.rs` implements `orrery_persistd::gateway::StandingInvalidationFeed`
importing nothing from `orrery_identity`, then has `orrery_identity`'s real
`FdbAccountStore::observe_cooldown` write a `dc` row and asserts the feed reads
back exactly what was written.

**It installs no feed and enforces nothing.** See the "What it deliberately
does not do" note at the top of `main.rs`: wiring a consumer makes the gateway
an enforcement point, and every enforcement point must read its C5 ramp
posture. #934 fixed a live bug caused by an enforcement arm that had none. The
posture read is left visibly undone rather than plausibly half-done.

## The cycle, reproduced

Adding the dependency the obvious way:

```diff
  # crates/orrery_persistd/Cargo.toml
  [dependencies]
  orrery_protocol = { path = "../orrery_protocol" }
+ orrery_identity = { path = "../orrery_identity" }
```

```
$ cargo metadata --format-version 1 --manifest-path crates/orrery_persistd/Cargo.toml
error: cyclic package dependency: package `orrery_identity v0.1.0 (…/crates/orrery_identity)` depends on itself. Cycle:
package `orrery_identity v0.1.0 (…/crates/orrery_identity)`
    ... which satisfies path dependency `orrery_identity` (locked to 0.1.0) of package `orrery_persistd v0.1.0 (…/crates/orrery_persistd)`
    ... which satisfies path dependency `orrery_persistd` (locked to 0.1.0) of package `orrery_identity v0.1.0 (…/crates/orrery_identity)`
    ... which satisfies path dependency `orrery_identity` (locked to 0.1.0) of package `orrery_coordinator v0.1.0 (…/crates/orrery_coordinator)`
```

**`optional = true` does not help.** The same command with

```toml
orrery_identity = { path = "../orrery_identity", optional = true }
```

produces the identical error, because cargo resolves the *package* graph before
it resolves features. This is the load-bearing detail: it is why the
coordinator's `standing-feed = ["dep:orrery_identity", …]` trick
(`crates/orrery_coordinator/Cargo.toml:36`) is available to the coordinator and
can never be available to persistd. The coordinator is downstream of identity;
persistd is upstream of it.

## Why the manifest is `Cargo.toml.txt`

Same reason as [`../d32-oq1-spike`](../d32-oq1-spike/README.md):
`check.sh --self-test` discovers every directory declaring `[workspace]` within
four levels of the repository root and dies on any that no lane visits
(`scripts/check.sh:635-648, 714-718`). A propose-only spike should not buy an
exemption from that rule. So the manifest ships inert.

Unlike the d32 spike, this one path-depends on workspace crates, so it must be
run **in place** — copying it to `/tmp` breaks the relative paths:

```sh
cd docs/spikes/862-gateway-consumer
cp Cargo.toml.txt Cargo.toml
FDB_CLUSTER_FILE=/path/to/.fdb-dev/fdb.cluster cargo run
rm -rf Cargo.toml Cargo.lock target        # leave the tree as you found it
```

## Result on 2026-09-03

Against the dev cluster at `127.0.0.1:4500`:

```
PASS: a dc row written by orrery_identity was read by a persistd-only feed
      [AccountInvalidation { account: AccountId(604045312905969665), effective_from_ms: UnixMillis(1756000000000) }]
      fixture rows cleared
```

Mutation, to show the assertion is not vacuous — point the range at `db`
(bindings) instead of `dc`:

```
thread 'main' panicked at main.rs:176:5:
assertion `left == right` failed: the gateway-side feed must see exactly what identity wrote
  left: []
 right: [AccountInvalidation { account: AccountId(604045312905969665), effective_from_ms: UnixMillis(1756000000000) }]
```

Account ids are drawn from `0x0862_0003_…`, a range no other lane uses, and the
spike clears its own `da` and `dc` rows on the way out. The dev cluster is
shared; a colliding fixture turns a sibling lane's test red.
