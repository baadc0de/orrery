# OD-22 — pricing the `RulesetId.digest` mechanism (propose-only)

**Issue:** #639 · **Status:** propose-only; no mechanism is selected or
implemented by this spike · **Measured:** 2026-08-31 on this checkout, Rust
1.96.0.

## Decision boundary

D49 clause (b) has already decided the meaning: `RulesetId.digest` is blake3
over the determinism-relevant source closure of the build — game crate(s) and
the first-party kernel crates they transitively depend on; source content is
read at the pinned toolchain, and the contributing-crate enumeration is part of
the input.  It expressly reserves only the computation mechanism: build
script, CI artifact, or lazy runtime hash ([D49](../adr/0049-compatibility-manifest.md#b-x-1--the-digests-scope-is-decided-its-mechanism-deliberately-is-not)).

This is therefore not a proposal to change the closure, `RulesetId`, peer
admission, or any placeholder.  The owner is choosing between these three ways
to make the already-decided value available:

| Choice | What supplies the 32 bytes to the existing `RulesetId` shape |
|---|---|
| Build script | `orrery_games` generates a Rust constant while Cargo builds it. |
| CI artifact | CI computes a commit-bound file and the release/build consumes it. |
| Lazy runtime hash | the linked game computes it once before exposing its ruleset identity. |

`RulesetId` remains `{ version: u32, digest: [u8; 32] }`
([`verifiable.rs:59-64`](../../crates/orrery_protocol/src/verifiable.rs#L59-L64)).
It is already embedded unchanged in frames and claims
([`verifiable.rs:179-214`](../../crates/orrery_protocol/src/verifiable.rs#L179-L214)),
and adjudication routes on equality with that shape
([`adjudication.rs:393-402`](../../crates/orrery_persistd/src/adjudication.rs#L393-L402)).
None of the options needs a wire, claim, bundle, or corpus-format change; only
the value stops being fabricated.

## Why a decision cannot wait

The history is not an inference from the issue.  `git log --follow -p` on
`crates/orrery_games/src/regolith/mod.rs`, inspected on this checkout, shows
every value below being hand-written beside an ordinary `version` bump:

| Commit (local time) | Version/digest transition | Change carrying it |
|---|---|---|
| `9f986e7` (2026-08-23) | v5 `[0x52; 32]` → v6 `[0x63; 32]` | tracking / hit-resolution work (#363) |
| `fbce95c` (2026-08-25) | v11 `[0x65; 32]` → v12 `[0x66; 32]` | recorded-frame collision work (#468) |
| `b82d6da` (2026-08-31 11:51) | v19 `[0x68; 32]` → v20 `[0x69; 32]` | snapshot isolation (#784) |
| `193f585` (2026-08-31 16:51) | v20 `[0x69; 32]` → v21 `[0x6A; 32]` | campaign crowd geometry (#799) |

The abbreviated `[0x63] → [0x66] → [0x69] → [0x6A]` account is true but
omits the same hand-bump pattern through `[0x64]`, `[0x65]`, `[0x67]`, and
`[0x68]`.  The final two changes occurred five hours apart on 2026-08-31.
The current source still says `digest: [0x6A; 32]`
([`regolith/mod.rs:349-351`](../../crates/orrery_games/src/regolith/mod.rs#L349-L351));
Skirmish still documents why its `[0x5C; 32]` is an honest placeholder
([`skirmish/mod.rs:98-105`](../../crates/orrery_games/src/skirmish/mod.rs#L98-L105)).

Thus this is no longer merely a missing feature.  A value manually advanced to
look like build identity is exactly D49's dangerous middle state: a peer sees a
well-formed, plausible 32-byte value, but there is no derivation to make it
mean the build it names.

## Closure that every candidate must implement

The present `orrery_games` package directly depends on the first-party
`orrery_compose`, `orrery_core`, and `orrery_protocol` packages (verified with
`cargo metadata --offline`).  Its D49 closure is consequently these four
crates.  The measurement inventory contains 52 production `.rs` files and
1,144,609 source bytes:

| Crate | Files | Source bytes |
|---|---:|---:|
| `orrery_games` | 21 | 447,399 |
| `orrery_compose` | 2 | 40,272 |
| `orrery_core` | 14 | 250,057 |
| `orrery_protocol` | 15 | 406,881 |

The implementation contract should be one shared, versioned closure encoder,
not three subtly different hashers.  Its input must contain, with length
prefixes and a domain tag:

1. the sorted first-party crate enumeration that Cargo resolves from the game
   package at the pinned toolchain;
2. for each crate, its logical crate name and the sorted logical path of each
   production Rust source unit; and
3. the canonical token stream of that unit, with ordinary comments and
   `#[cfg(test)]` modules omitted.

This includes a changed first-party dependency because the resolver's sorted
crate enumeration changes, and includes an edit in one because its production
tokens change.  It excludes integration/unit tests, docs, presentation assets,
operator configuration, environment, timestamps, Git revision, target, and
third-party crate source.  Those exclusions implement D49's “nothing outside
the closure” boundary; including a Git SHA or all of `Cargo.lock` would make a
digest move for information D49 did not put in scope.

The prototype used the four current crates and a deliberately explicit
enumeration only to measure the hashing work.  A landing must **not** retain a
hand-maintained list as its authority: the build-time resolver and a CI check
must compare the selected first-party transitive closure with
`cargo metadata --locked --offline`.  Otherwise adding a first-party kernel
dependency is the build-script version of the stale-artifact lie.

Because both games currently live in `orrery_games`, this crate-level D49
closure is the same for Regolith and Skirmish.  A Regolith-only edit therefore
also changes Skirmish's digest: conservative incompatibility, never a false
claim of compatibility.  A third ruleset in this crate works the same way; a
third ruleset in a new game crate gets that crate plus its resolved first-party
transitive closure.  Splitting identities more finely than a game *crate* is a
scope question, not a mechanism optimization, and is out of this spike.

## Measurements

I built a disposable, offline Cargo probe outside the worktree.  It walks the
52-file inventory, parses each Rust file to a token stream, excludes top-level
`#[cfg(test)]` modules, length-prefixes the enumeration and file records, and
blake3s the stream.  The identical hasher was exercised as a build script and
as a release executable.  No game placeholder was replaced; all temporary
source mutations were reverted.

| Measured operation | Result | What it prices |
|---|---:|---|
| Release executable hashes closure, five runs | 53–55 ms each | lazy first-use CPU; CI artifact generation CPU |
| Probe cold release build, including `blake3`, `syn`, and `quote` | 4.04 s | unassisted cold bootstrap of a source-token hasher |
| Build-script probe, no-op rebuild | 22 ms | Cargo bookkeeping when no input changed |
| Build-script probe after production source mutation | 0.29–1.09 s | observed rebuild envelope, including invoking Cargo and recompiling the probe crate |

The probe was intentionally outside this checkout, so its 4.04 s cold figure
did not use the repository's `.cargo/config.toml` `kache` wrapper.  In the real
tree `kache` caches rustc objects, but not linking or `build.rs` executions
([`build-cache.md:117-128`](../build-cache.md#L117-L128)); the 53–55 ms hash is
therefore paid whenever the build script reruns.  It should not be represented
as a whole-workspace cold-build delta.  Measuring that would require two
isolated full game builds while other lanes are compiling, which this spike
did not do.  The attributable, measured lower-level costs above are the
numbers the owner needs; the selected implementation PR should record its
actual `orrery_games` cold/warm deltas.

## Price by mechanism

| Mechanism | What it hashes / dependency change | Staleness and peer-visible failure | Cost and `kache` | Unavailable environment | Both games / third game |
|---|---|---|---|---|---|
| **Build script** | Runs the shared closure encoder during `orrery_games` compilation and emits a generated constant.  It emits `cargo:rerun-if-changed` for every selected source and verifies its selected crate set against Cargo metadata. | Stale only if the resolver or rerun set omits an input.  The peer gets an old but plausible 32-byte digest — D49's bad case.  The mandatory metadata comparison and mutation test make that a fail-closed CI error, not a silent success. | 53–55 ms whenever the script runs; 22 ms measured no-op probe bookkeeping.  The source-token build dependencies cost 4.04 s without cache; kache should reuse their rustc units, but cannot cache the script's own execution. | Works in fresh clones and `cargo build --offline`: source is already present; no CI/network is consulted.  If the pinned hasher/parser dependency is absent locally, Cargo fails rather than inventing a value. | Yes.  Today both use the four-crate closure.  New crate means resolver discovers a new closure; new module in `orrery_games` remains conservative. |
| **CI artifact** | CI runs the same encoder and uploads a file containing digest, commit/tree identity, toolchain, and closure inventory.  The consumer must reject an artifact whose source identity differs. | This is the highest stale risk: a checked-in file, an artifact selected by branch/tag rather than exact source, or a local fallback can all inject a plausible old digest.  Exact source identity and mandatory verification reduce the risk, but a failed/missing artifact still cannot be treated as a digest. | Local generation is the same measured 53–55 ms.  Upload/download and queue time are CI/network properties and were not measurable without creating a CI artifact; they are additional, not zero.  It avoids a local parser dependency only for builds that never need to create or verify the value. | Does **not** work for a fresh offline clone unless the artifact is already packaged with the exact source.  Failing closed is safe; falling back to the current constant or a prior artifact is not. | One artifact must carry one closure result per game build.  A third ruleset requires CI inventory and release packaging changes; an omitted entry is a safe failure only if the consumer refuses to build. |
| **Lazy runtime hash** | Linked game code retains the source/token inputs (for example `include_bytes!` plus the shared encoder) and hashes them once through `OnceLock` before returning its ruleset id. | No artifact can be stale, but stale source inclusion is possible if the embedding list/resolver omits a file.  The peer then sees a plausible wrong digest.  The same metadata check and mutations are required. | Measured first-use hash is 53–55 ms.  At least 1.14 MB of present production-source input must ship/be embedded before encoder overhead; source inclusion also recompiles consumers when it changes.  `kache` can cache compilation but not that first runtime work. | Works offline only if the exact source closure is shipped beside or embedded in the binary.  If it reads the checkout, installed releases lack source and must fail. | Yes, but both games need separately callable non-`const` identity construction.  A third game needs another embedded closure and increases binary payload. |

### Mutation check — build-script prototype

This is the most fully exercised candidate.  Its output was the following
digest before each mutation:

```
68c4adc71ae1e85e7f07678b810df1efde561b5ecd8a9a71341f7cf216caa2d7
```

| Mutation, then rebuild | Observed digest | Result |
|---|---|---|
| Change Regolith's production `DT` expression from `1.0 / TICK_HZ` to `1.0 / (TICK_HZ + 1)` | `9392cb4f894f5fa60f8ee91021f1e0766207c7a68e86f50e2347bb0de46392a6` | **Changed** — non-vacuous sensitivity. |
| Add an ordinary comment next to that production expression | baseline value | **Unchanged** — token encoding ignores it. |
| Add a marker in `crates/orrery_games/tests/regolith.rs` | baseline value | **Unchanged** — integration tests are not enumerated. |
| Add a test-only const inside Regolith's `#[cfg(test)] composition_tests` module | baseline value | **Unchanged** — top-level `cfg(test)` modules are removed before token encoding. |

The build-script probe also printed one `rerun-if-changed` directive per
selected source file.  That detail is essential: the first prototype version
did not, which would allow a transitive-core change to leave a generated output
unrefreshed.  That is a rejected form of the build-script option, not a minor
optimization.  The mutations above were all reverted; `git status` was clean
before this document was added.

The test filter is intentionally only a measured proof for the current source
layout, where every `#[cfg(test)]` occurrence is a top-level module.  The
landing must make test-target exclusion a general parser/resolver property and
add this mutation as a permanent check.  A hash that simply reads every `.rs`
byte fails the comment and test requirements even though it produces changing
hashes; it is not acceptable.

## Corpus and delivery cost

Whichever mechanism lands, the PR must regenerate
`crates/orrery_conformance/corpus/golden.json`'s `ruleset_digest` in a
separate commit explicitly labelled **golden regeneration**.  The current
field is the 32-byte `c0…c0` value
([`golden.json:1-5`](../../crates/orrery_conformance/corpus/golden.json#L1-L5)).
The established tool exposes `orrery-conformance emit --out <file> --compact`
([`main.rs:43-72`](../../crates/orrery_conformance/src/main.rs#L43-L72)).

That action is deliberately not run here: it writes the committed corpus,
whereas this spike makes no identity change.  Its eventual cost is one corpus
execution plus review of the labelled JSON update; it must be executed in the
implementation PR so its report is tied to the selected value, not copied from
this proposal.

## Recommendation — owner may reject

Choose **build script**, provided the implementation PR has all of these
non-negotiable acceptance checks:

1. one shared closure encoder, a Cargo-metadata closure check, and explicit
   rerun directives for every selected input;
2. offline fresh-clone build succeeds and a missing hasher/parser fails the
   build rather than using any fallback value;
3. permanent rule/comment/integration-test/embedded-test mutation checks; and
4. the separately-labelled corpus regeneration commit.

It is the only option that supplies the decided value at the honest build
point, with the measured steady-state hash cost in tens of milliseconds, while
remaining usable without CI or network.  CI artifact is rejectable on the
evidence here because its safe form fails local/offline builds and its
convenient forms reintroduce D49's plausible stale value.  Lazy runtime hash
is technically viable but pays the same measured hash late, forces source
payload/non-`const` identity plumbing, and provides no benefit over generating
the same bytes during the build.

This recommendation selects no code in this branch.  It prices the owner’s
three reserved alternatives and makes the required safety bar explicit.
