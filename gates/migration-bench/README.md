# gates/migration-bench

A10 F-12 (stage S1.e of the [#626](https://github.com/baadc0de/orrery/issues/626)
programme): the migration benchmark harness, and the rule it makes
enforceable — **no differential run without a committed baseline.**

[A10 §8](../../docs/plans/a10-conformance-benchmarks.md) puts it as: a
comparison against nothing is worse than no comparison, because it produces a
number. Every later phase-exit evaluation in the programme compares against a
baseline; this crate is the harness that refuses to let one happen without
it, and the holder of the first baseline.

Listed in `scripts/check.sh`'s `WORKSPACES` table with role `check` — the
[`p2-journal-bench`](../p2-journal-bench) precedent: CI compiles it, so it
cannot rot, and never executes it, so it cannot flake. Execution is A10
§8.3's procedure, by hand, at capture and at phase exit.

## What it refuses

`compare` is the differential run. It refuses — before running anything, with
nothing on stdout — in exactly two cases:

| Exit | Verdict | When |
|---|---|---|
| 3 | `no committed baseline` | No file at `--baseline <path>`, or the file is not a valid baseline document. The refusal names the path. |
| 4 | `environment mismatch` | The baseline exists, but its environment manifest disagrees with this environment on `rustc_version`, `target_triple`, `build_profile`, or `cpu.model`. The refusal names every differing field. |

Matching is deliberately narrow. Commit is *not* matched — a differential
comparison is a comparison across commits; that is its entire purpose. The
`Cargo.lock` digest is *not* refused — a dependency bump inside the D14 pins
warns, loudly, but does not block. Everything else the manifest records
(RAM, cores, OS) is provenance, not a gate. Every field added to the refuse
set turns the next environment change into a new baseline, and a rule that
refuses too often stops being a rule and becomes a ritual.

## What it measures

The suite drives the conformance population itself — the corpus cases and
the scenario battery — so a number here is about a workload the fixtures
cover (A10 §8.2). Repetition counts are fixed constants, not time budgets.

| Leg | Instrument | Metric |
|---|---|---|
| B-1 `b1.corpus.*` | `orrery_conformance::run_case`, 31 timed runs per case | full-population tick µs and per-entity-tick µs, p50/p99 |
| B-1 `b1.battery.*` | `orrery_games::scenario::play`, 11 timed plays per game × scenario | same, plus event counts (workload shape) |
| B-3 `b3.memory.*` | RSS high-water delta, 9 runs of the largest committed corpus case (Linux only) | bytes per entity |
| B-4 `b4.snapshot.*` | `CoreCodec::to_canonical` over the battery's logged states | encode µs p50/p99, canonical bytes |
| B-5 `b5.claim.*` | clone → quantize → canonical encode → blake3, the `StateClaim` path | µs per claim, p50/p99 |

Legs whose instrument does not exist are recorded **absent with a reason**
in the baseline document, never silently skipped: `b1.swarm-large` (waits for
S1.c's 256-entity case), `b2.structural-change` (no Split-storm scenario in
the committed battery), `b4.corpus-checkpoint` (`run_case` does not expose its
final states; the same canonical encode is measured over battery states
instead), `b4.feed-uplink-diff`, `b6.startup` (Phase 2), and
`b7.compile-and-binary-size` (needs an isolated cold target directory; this
box shares one kache cache and one disk with live lanes).

## What it does not do

**No thresholds.** `compare` emits ratios, both environment manifests, and a
literal field saying so. There is no pass, no fail, no exit code tied to a
number. Thresholds (A10 §8.4's proposed values) are evaluated by a human at
phase exit, on the same host class, against the committed baseline — never
asserted by CI, where a flaky bench becomes a blocked merge.

**No writing into `docs/plans/baselines/`.** `capture` emits a candidate (to
stdout or `--out`); a human reviews and commits it. The committed file is the
unit of trust, and the harness never edits it.

## Running it

```sh
cargo build --release
# observe-only candidate (this is how the committed baseline was produced):
./target/release/migration-bench capture
# the differential run:
./target/release/migration-bench compare \
    --baseline ../../docs/plans/baselines/a18-baseline-2026-08-30.json
```

Compare against a missing path and the harness exits 3 without producing a
report; compare from a different toolchain, target, profile or CPU and it
exits 4 the same way. Exit 0 means a real comparison happened and produced
ratios.
