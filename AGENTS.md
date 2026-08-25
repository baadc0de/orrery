# AGENTS.md

Guidance for AI coding agents working in this repository.

## What this is

**Orrery** is an in-development set of Rust crates for the Bevy game engine (0.19):
peer-to-peer multiplayer (QUIC transport with NAT hole punching via iroh) and a
persistent-universe backend. The repository contains the accepted architecture,
active P0–P4 implementation — P4 landed as construction and open as
*measurement*, with witnessing in shadow mode — test tools, and incomplete
milestone harnesses.

## Reading path (normative order)

The design is documented in `docs/`. Accepted ADRs are normative over the
README and every numbered expansion document.

Start with [docs/DECISIONS.md](docs/DECISIONS.md), the ADR index and governance
entry point. Decisions live independently under [docs/adr/](docs/adr/):

- For architecture-wide work, read all accepted ADRs in numeric order.
- For scoped work, read the index, the ADRs named by the relevant expansion
  document, and any ADR dependencies those records link.
- Never treat the index summary as a substitute for the applicable ADR text.
- A future change to an accepted decision is a new ADR that explicitly
  supersedes the old one; do not silently rewrite architectural history.

| Decision | ADR | Covers |
|---|---|---|
| D1 | [ADR-0001](docs/adr/0001-requirements.md) | Requirements |
| D2 | [ADR-0002](docs/adr/0002-simulation-model.md) | Simulation model |
| D3 | [ADR-0003](docs/adr/0003-transport.md) | iroh transport |
| D4 | [ADR-0004](docs/adr/0004-bevy-netcode-stack.md) | Bevy netcode stack |
| D5 | [ADR-0005](docs/adr/0005-spatial-model.md) | Spatial model and CellId |
| D6 | [ADR-0006](docs/adr/0006-population-adaptive-topology.md) | Island topology |
| D7 | [ADR-0007](docs/adr/0007-authority-and-leases.md) | Authority and leases |
| D8 | [ADR-0008](docs/adr/0008-prediction-rollback-interpolation.md) | Prediction and rollback |
| D9 | [ADR-0009](docs/adr/0009-verifiable-core.md) | Verifiable core |
| D10 | [ADR-0010](docs/adr/0010-witnessing.md) | Witnessing |
| D11 | [ADR-0011](docs/adr/0011-persistence.md) | Persistence |
| D12 | [ADR-0012](docs/adr/0012-backend-services.md) | Backend services |
| D13 | [ADR-0013](docs/adr/0013-physics-and-determinism.md) | Physics posture |
| D14 | [ADR-0014](docs/adr/0014-pinned-versions.md) | Pinned versions |
| D15 | [ADR-0015](docs/adr/0015-crate-set.md) | Crate set |
| D16 | [ADR-0016](docs/adr/0016-parameter-reference.md) | Parameter defaults |
| D17 | [ADR-0017](docs/adr/0017-risks-and-open-questions.md) | Risks and open questions |
| D19 | [ADR-0019](docs/adr/0019-indexed-waldb-journal.md) | Indexed wal-db journal default |
| D20 | [ADR-0020](docs/adr/0020-journal-retention.md) | Journal retention and the recovery budget |
| D21 | [ADR-0021](docs/adr/0021-ruleset-distribution.md) | `Ruleset` distribution, harness API freeze |
| D22 | [ADR-0022](docs/adr/0022-grid-id-in-the-storage-key.md) | `GridId` stays a key discriminator |
| D23 | [ADR-0023](docs/adr/0023-follower-journal-retention.md) | Follower journal retention, P2 retention clause |
| D24 | [ADR-0024](docs/adr/0024-island-drain.md) | Island drain is peer-driven; no evacuation |
| D25 | [ADR-0025](docs/adr/0025-expire-fan-out.md) | `Expire` fan-out set and bound |
| D26 | [ADR-0026](docs/adr/0026-sibling-gateways.md) | Sibling gateways: ownership, reachability, live handover |
| D27 | [ADR-0027](docs/adr/0027-attestation-envelope.md) | Attestation envelope and required-K draw |
| D28 | [ADR-0028](docs/adr/0028-witness-set-seeding.md) | Witness-set seeding, announcement, `epoch/` record |
| D29 | [ADR-0029](docs/adr/0029-low-population-path.md) | P5 low-population path: provisional commit, spot replay, annulment |
| D30 | [ADR-0030](docs/adr/0030-cell-epoch-standing.md) | Cell-epoch standing: which announced set may judge an intent (Proposed) |

After the applicable ADRs, use this expansion reading path:

| Order | Document | Covers |
|---|---|---|
| 1 | [docs/00-overview.md](docs/00-overview.md) | Goals, constraints, system diagram, subsystem tour, glossary |
| 2 | [docs/01-spatial-model.md](docs/01-spatial-model.md) | Grid, `CellId` encoding, `big_space`, AOI, hysteresis, hotspots |
| 3 | [docs/02-networking.md](docs/02-networking.md) | iroh, relays, islands, topology regimes, channels, bandwidth |
| 4 | [docs/03-replication.md](docs/03-replication.md) | replicon/lightyear stack, interest sets, delta compression, priority |
| 5 | [docs/04-authority.md](docs/04-authority.md) | Weak/strong claims, leases, handoff, orphans, promotion |
| 6 | [docs/05-prediction-rollback.md](docs/05-prediction-rollback.md) | Timelines, prediction sets, reconciliation, interpolation, hit validation |
| 7 | [docs/06-verifiable-core.md](docs/06-verifiable-core.md) | `Ruleset`, determinism scoping, signed input logs, replay harness |
| 8 | [docs/07-witnessing.md](docs/07-witnessing.md) | Threat model, discrepancy protocol, adjudication, strikes |
| 9 | [docs/08-persistence.md](docs/08-persistence.md) | Cell actors, journal, FDB schema, intents, terrain, event archive |
| 10 | [docs/09-services-and-ops.md](docs/09-services-and-ops.md) | Service inventory, deployment, scaling, failure modes, telemetry |
| 11 | [docs/10-crates.md](docs/10-crates.md) | Workspace layout, per-crate API sketches, dependency graph |
| 12 | [docs/11-roadmap.md](docs/11-roadmap.md) | Build phases (P0–P6), milestones, tracked risks |
| 13 | [docs/12-world-seeding.md](docs/12-world-seeding.md) | World seeder: TOML scenario runner, generator bank, content diff/patch (expands 08 §17) |
| 14 | [docs/13-chain-replication.md](docs/13-chain-replication.md) | Cross-process journal mirroring, reconnect, and recovery |
| 15 | [docs/14-capacity.md](docs/14-capacity.md) | Measured single-box capacity envelope: the knee, what binds, and when you have outgrown one box |
| 16 | [docs/references.md](docs/references.md) | Annotated bibliography, organized by topic |

Also read [README.md](README.md) — it summarizes the architecture, the status,
and the feature set.

## Ground rules

- **Accepted ADRs are normative.** The records in `docs/adr/` govern the
  README and numbered docs. If an expansion conflicts with an applicable ADR,
  the ADR wins.
- **Implementation is partial.** Inspect the current tree before assuming a
  designed crate or service exists. Code sketches in `docs/10-crates.md` are
  indicative of shape, not guaranteed to match landed APIs.
- **Pinned versions ([D14](docs/adr/0014-pinned-versions.md)).** All dependency
  versions reflect the ecosystem as of August 2026 and are re-validated when
  implementation starts. Don't bump them casually.
- **Roadmap gates ([D17](docs/adr/0017-risks-and-open-questions.md)).** Each
  phase (P0–P6) has a demo criterion that is a permanent regression harness and
  gates entry to the next phase. See [docs/11-roadmap.md](docs/11-roadmap.md).

## CI

`.github/workflows/ci.yml` runs on every push and pull request: `rustfmt`,
`clippy -D warnings`, the verifiable-core static gates
(`scripts/core-gates.sh`), every `--self-test` mode in `scripts/`, the
workspace test suite, the standalone tools' own test suites, and the
cross-platform determinism matrix.

### Running it here: `scripts/check.sh`

Four of those jobs — `fmt`, `clippy`, `gates`, `test` — have no command bodies
of their own any more. They install a toolchain, apt packages and the
FoundationDB client, restore a cache, set the rustc wrapper, and then invoke
one lane of `scripts/check.sh`. So the way to find out whether a change passes
is to run it, not to push and wait:

```
./scripts/check.sh              # every lane, in CI's order
./scripts/check.sh clippy       # one lane, exactly the commands the job runs
./scripts/check.sh --list       # what a lane would run, without running it
./scripts/check.sh --self-test  # the lane table and the self-test coverage hold
./scripts/check.sh doctor       # delegates to dev-cache.sh: is the cache working?
```

**Scope the claim, because overclaiming it is how it stops being trusted.**
The script is the body of those four jobs and nothing else. `determinism` and
`determinism-verdict` keep four more cargo commands — a cross-platform matrix
is not something one machine can reproduce — and `nightly.yml` carries six
cargo invocations of its own plus the four gate scripts it runs *for real*,
including the two heavy harnesses that need an FDB cluster and eight peer
processes. Neither workflow is reproduced here.

**The hosted measurements are the current capacity evidence, not local
timing promises.** On `ubuntu-latest`, #171 measured cold, cacheless wall
times of 239/244 s for `clippy`, 656 s for `gates`, and 674/681 s for `test`;
the corresponding peak free disk was 82.78, 68.64, and 39.29 GiB. The current
`clippy` remote-cache samples (2026-08-21, unchanged tree) were 148/131/121 s
with 99.9/99.9/100.0% hits and 0.460/0.461/0.461 GiB pulled. Those figures
describe those hosted runs and their cache posture, not this workstation or a
future change: time a particular change where it will run and record the
runner and cache state with the result.

### Twelve workspaces, and only one of them is "the" workspace

`cargo test --workspace` reaches the root workspace. Each standalone tool
declares its own `[workspace]` table, so it reaches none of *them* — three red
CIs in one week came from that blind spot. The inventory, which is also
`scripts/check.sh`'s lane table:

| Workspace | Role in the lanes |
|---|---|
| `.` (root, 15 first-party crates + 3 vendored) | `clippy` and `test` lanes; `fmt` like any other |
| `gates/p1-swarm` | `cargo test` in `gates` |
| `gates/p2-load` | `cargo test` in `gates` |
| `gates/p2-dashboard` | `cargo test` in `gates` |
| `gates/p4-streams-bench` | `cargo test` in `gates` |
| `gates/p0-nat-test` | `cargo check --all-targets` — no tests |
| `gates/p0-dashboard` | `cargo check --all-targets` — no tests |
| `gates/p3-island` | `cargo test` in `gates`; asserted by the nightly island gate |
| `gates/p3-siblings` | `cargo test` in `gates`; the two-gateway harness, asserted by the nightly sibling gate. The only tool that links `libfdb_c` besides `gates/p2-load`: its double-spend race leg reads the ledger back out of FoundationDB |
| `gates/p5-dupe-gauntlet` | `cargo test`; the single-gateway replay, attestation-abuse and quarantine proof, asserted by the nightly P5 gate against FoundationDB. Its `ramp` subcommand carries D32's enforcement-ramp arms too, asserted by the nightly `ramp-shadow` gate, which runs a shadow and an enforcing gateway from this one binary at the same time. Its additive measurement tests pin #153's p99 population and stage-count guards |
| `gates/p2-journal-bench` | `cargo check --all-targets` — no tests |
| `clients/regolith` | `cargo test` in `gates`; the Bevy 0.19 keyboard/rendering skin over the headless Regolith intent and replay pipeline |

The eight standalone test suites, and the three tools checked without tests, are the work
that would go unrun if the `gates` lane stopped visiting them. Do not hand-copy
test totals here: `./scripts/check.sh --list` is the executable inventory.

`--self-test` compares that table against the filesystem — every directory
whose `Cargo.toml` declares `[workspace]` must appear in it — so a thirteenth
workspace cannot be added and silently go unchecked. It is a two-source check
by construction: the table cannot match itself.

**`fmt` is where this actually bit.** `cargo fmt --all` means "every member of
*this* workspace", so the root-only invocation the workflow used to run reached
zero of the 27 `.rs` files under the standalone tools. Widening it found exactly one
dirty workspace, `gates/p0-nat-test`. Note the flip side at the root: the three
vendored crates *are* root members, so `cargo fmt --all` holds `vendor/` to
default rustfmt even though clippy deliberately excludes it.

### A `--self-test` nothing runs is not a check

A second clause covers the gate scripts themselves.
`scripts/p4-accumulate.sh` and `scripts/p4-ledger.sh` each grew a `--self-test` that only `nightly.yml` ever
called, and the cost was not hypothetical: `p4-ledger.sh --self-test` counted
ledger lines with `wc -l`, BSD `wc` pads its output to a fixed width, and the
nightly's macOS leg failed on `[[ "       1" == 1 ]]` on every run until it was
found. No per-commit check invoked it, so nothing could have caught it earlier.

So `check.sh --self-test` now `find`s every script in `scripts/` that
*dispatches* on `--self-test` and asserts the `gates` lane invokes each one.
The two sides are the filesystem and the text of `lane_gates`, which is what
makes it a check rather than a restatement — a clause reading both out of one
list would pass on exactly the script nothing runs. Adding a gate script with a
self-test and forgetting to wire it up now fails the gate.

**Writing one that can fail.** Structural self-tests grep the script's own body
for the stages that make it a proof, and the failure mode is subtle: a pattern
that also appears somewhere harmless — a `${VAR:?usage}` message, an output
path template, another leg of the same harness — passes on a script that has
lost the stage entirely. The rule is to mutation-check each clause: break the
*guarded stage*, confirm the self-test fails, restore. Breaking the stage and
the check line together proves nothing, which is how vacuous clauses survive.

**`CARGO_TARGET_DIR` is never exported by the script.** An already-set value
always wins, and `--isolate` (per-lane directories, local use only) is opt-in.
An agent harness sets one per task; an unconditional export would collapse
isolated lanes onto a single exclusively locked directory, so they would queue,
not merely share.

Five things about the workflow itself are worth knowing before you change
anything it touches.

**`clippy` is enforced at `-D warnings`, over three feature sets.** The default
indexed-raw build, the `fdb` build, and D19's explicit Fjall fallback compile
different code, and the `fdb` half once went
unlinted long enough for `orrery_seed/tests/fdb_gates.rs` to stop compiling
altogether. All three are gated now. `clippy` needs only metadata, so the `fdb` pass
runs with no `libfdb_c` on the runner. Vendored crates under `vendor/` are
excluded — their findings are upstream's to fix — and the run passes
`--no-deps`, without which `--exclude` does not actually spare them: they are
still path dependencies, and clippy lints those too. The workspace test job
excludes the same three, then runs `orrery_persistd`'s library tests once more
with `--no-default-features --features journal-fjall,chain-grpc`: the default
suite covers raw, clippy compiles every fallback target, and the Fjall unit
tests are not allowed to rot. The full fallback integration suite is not run a
second time because it retains Fjall's known shutdown-hang exposure.
`bevy_replicon`'s own tests and doctests do not compile under this workspace's
feature unification, because `bevy/serialize` is off and they need
`Transform: Serialize`.

One thing to know while you are in there: **`[workspace.lints]` still reaches
only the vendored crates.** `vendor/aeronet_iroh`, `vendor/aeronet_tokio_runtime`
and `vendor/bevy_replicon` are the only manifests with `[lints] workspace = true`,
so the `pedantic`/`nursery`/`missing_docs`/`unwrap_used` levels configured at the
workspace root apply to third-party code and to none of `crates/*` through that
mechanism. That is backwards, and adopting the table wholesale is still its own
piece of work.

But do not read it as "no lint levels apply to `crates/*`", because one of them
does and it bites. All fifteen first-party crates set
`#![warn(missing_docs)]` in their own `lib.rs`, and CI runs `clippy --workspace
--all-targets --no-deps -- -D warnings`, which promotes that warning to an
error. **An undocumented
public item fails CI today.** What is genuinely unadopted is the rest:
`pedantic`, `nursery` and `unwrap_used` have never been enforced on first-party
code, and turning them on is the large piece of work.

**Determinism is checked *across* platforms, not on each one.** `orrery_core`'s
own tests run an identical tick twice in-process and compare hashes, which
catches VC-4/VC-8 violations but only ever proves one platform agrees with
itself. `orrery_conformance` closes that: it runs a fixed corpus through a
reference ruleset on the four supported targets (x86_64 Linux/Windows,
aarch64 Linux/macOS — x86_64 macOS is one of docs/06 §8's five, deliberately
dropped as unsupported), emits a digest of per-tick state hashes, and a
final job compares them. Discrete state must be bit-identical; a mismatch is
localized to the first diverging `(tick, entity)` and quantified against the §5
bands so you can tell `libm` drift from a rules change.

`crates/orrery_conformance/corpus/golden.json` is committed, and every platform
also checks against it inside the ordinary test suite. **If you change the
reference ruleset, bump `REFERENCE_RULESET.version` and regenerate the golden:**

```sh
cargo run -p orrery_conformance -- emit --out crates/orrery_conformance/corpus/golden.json --compact
```

Regenerating without bumping the version hides a rules change as a determinism
pass, which is the one failure this whole apparatus exists to prevent.

`orrery_games` — the reference games P4 measures against — rides the same
matrix and carries the same obligation. Its golden chains live in
`crates/orrery_games/src/golden.rs`, one per scenario per game, and every
target checks them inside the ordinary test suite. **If you change a game's
rules, its pilot, its spawns or its scenario table, bump that game's
`RulesetId` version and regenerate:**

```sh
cargo test -p orrery_games --test battery -- --ignored --nocapture emit_goldens
cargo fmt -p orrery_games
```

`scripts/core-gates.sh` scans `orrery_games` and `orrery_conformance`
alongside `orrery_core` — the determinism rules are about the rules code, so a
`HashMap` or a `SystemTime::now` inside a `Ruleset` fails the same gate it would
in the core. A fourth clause, scoped to the two ruleset crates, refuses a live
neighbour read: cross-entity effects travel as events, because the adjudicator
installs exactly one entity and a neighbour read is always `None` at replay.

**The Regolith client is deliberately not in that matrix** (#344; #327 tried it
and both Linux legs died on `wayland-client`). The matrix asserts one thing:
the headless spine producing identical bytes on four platforms, which is why
its legs install no graphics dependencies. A render/input skin makes no
determinism claim — #320's constraint 3 puts input source and rendering as the
only deltas from the bot path — so it has nothing to assert there and would
cost the matrix its headlessness. Its home is instead: Linux per commit in
`scripts/check.sh`'s WORKSPACES table (role `test`, run by the `gates` lane),
and Windows/macOS per commit in `ci.yml`'s `client-platforms` job, whose legs
assert via `scripts/client-tests.sh` — an executed-test floor plus a check
that the client's own unittest binary ran, because a compile-heavy Bevy job
can otherwise pass with zero tests executed or with another workspace's suite
standing in. The job carries no apt packages (those are Linux-only build
inputs) and caches no `target/` (a Bevy-sized one per platform would evict the
determinism legs' caches under the shared 10 GB Actions allowance). The job's
legs go live when `clients/regolith/` exists on the checked-out tree, so its
merge order relative to PR #342 does not matter.

**All workflow jobs run on GitHub-hosted runners.** `ci.yml` and `nightly.yml`
name `ubuntu-latest`, `windows-latest`, `macos-latest`, or a matrix value for
one of those; neither names a self-hosted label. GitHub reports zero registered
runners for this repository. The workflow-level `RUSTC_WRAPPER: ""` is the
safe default for an ephemeral runner; a job may install and configure its own
cache within that run, but it must not rely on a persistent runner `target/`.

The `ci` Unix account on `orrery-hel1-1` is idle: it no longer runs GitHub
Actions. Do not administer runner services there; the `actions.runner.*` units
are gone.

The jobs that need a FoundationDB *server* — `p2-kill9`, `gates/p3-siblings`,
`gates/p5-dupe-gauntlet`, `ramp-shadow` and `fdb-tests`, all in `nightly.yml` — provision one per
run through the composite action
[`.github/actions/foundationdb`](.github/actions/foundationdb/action.yml) with
`server: "true"`, points `ORRERY_FDB_CLUSTER_FILE` at the package-configured
`/etc/foundationdb/fdb.cluster`, writes into whatever cluster it is given, and
discards it with the runner. There is no long-running CI cluster for a gate to
be mis-pointed at — see [Working alongside other agents](#working-alongside-other-agents).

`gates/p3-island` contains no FoundationDB reference, binds every listener on
`127.0.0.1:0`, and runs persistd with `--allow-volatile-leases`; it runs on a
GitHub-hosted runner with the other nightly jobs.

The heavy harnesses — P2's kill-9 gate, which needs a real FoundationDB
cluster, and P3's island gate, which needs eight peer processes and a real
`kill -9` — cannot gate a pull request. They run nightly and on demand in
`.github/workflows/nightly.yml`, alongside a soak that repeats the corpus ten
times in one process to catch per-process nondeterminism.

**The `fdb` feature has its own nightly test job, and a wrapper script is the
reason it means anything.** `orrery_persistd` and `orrery_seed` carry a tier of
tests that only compile under `--features fdb`, and every one of them opens
with a guard that `eprintln!("skipping: …")` and returns `Ok` when it cannot
find a cluster. That guard is right for a developer's `cargo test` and a trap
for CI: `cargo test --features fdb` on a runner with no cluster is 27 passes
that assert nothing. So `scripts/fdb-tests.sh` runs them — both packages in one
invocation, per C-8 — captures stderr with `--nocapture`, fails on any
`skipping:` line, and asserts a floor on how many tests actually executed. Its
`--self-test` proves those assertions against six synthetic logs and runs
per-commit, alongside every other self-test in `scripts/`.

**The standalone tools are tested per-commit too.** Each declares its own
`[workspace]`, so `cargo test --workspace` reaches none of them; the `gates`
lane runs `cargo test` in its eight test workspaces and `cargo check --all-targets`
in its three check-only workspaces — see the inventory above, which is the table
the lane iterates.
`gates/p2-load` takes `orrery_persistd` with `features = ["fdb"]`, which is why that
job installs the FoundationDB *client* on the hosted path.

### Where every gate stands: `scripts/gate-status.sh`

There was no single way to find out what every gate in this repository
currently says. The phase gates (P1 swarm, P2 kill-9, P3 island, P4
accumulate/ledger), the static gates, the `fdb` tier and the determinism corpus
report in different places — `scripts/*.sh`, two workflows, and the evidence
directories the nightly uploads — so answering "where does every gate stand,
with numbers" meant reading five files and running things by hand. Nobody does
that, which is how a vacuous self-test and an unrun `p4-*` self-test both
survived for weeks.

```
./scripts/gate-status.sh              # --fast: static gates + every --self-test
./scripts/gate-status.sh --full       # also every harness whose prerequisites hold here
./scripts/gate-status.sh --inspect    # run nothing; report from evidence on disk
./scripts/gate-status.sh --self-test  # its own structural + functional check
```

**The gate list is discovered, never typed.** Scripts come from a `find` over
`scripts/`, jobs from the `jobs:` keys of `nightly.yml` and `ci.yml`, and the
static gates from the text of `check.sh`'s `lane_gates`. A typed inventory is
the thing that rots: it stays green while the tree moves under it. What the
script does hold is one `gate_<key>_{tier,prereq,run,evidence}` trio per gate it
knows how to run and read — and a gate discovered with no trio is reported
`UNKNOWN` and exits 2. So adding a gate and forgetting this script breaks the
report loudly instead of dropping a gate from it silently.

**A skip is never a pass.** Five statuses that do not collapse into each other:
`PASSED`, `FAILED`, `NOT RUN` (runnable, this mode did not run it, no evidence
on disk), `SKIPPED` (a prerequisite is missing — no cluster, no hosted runner —
so the gate was *not evaluated* and nothing is claimed), and `UNKNOWN`. The
exit status carries the same distinction: 0 nothing failed, 1 a gate failed, 2
the report has a hole in it. A run that skipped every heavy harness exits 0 and
says so in every line of its summary; it does not say the gates passed.

**Numbers come out of the gates' own reports**, never re-derived: settle times
and disposition counts from `p3-island-*/report.json`, player-hours and
false-positive counts from `target/gates/p1-swarm/*.json`, latency percentiles and
the recovery cutoff from `p2-kill9-*/artifact.json`, banked hours from the P4
ledger, executed test counts from `target/fdb-tests.log`. A figure this script
computed itself would be a second implementation of the gate, and the two would
disagree exactly when it mattered.

**The mode is printed in the banner, in the summary, and in every JSONL
record**, because a fast run must never be read as a full one. The machine-
readable half is `target/gate-status/gate-status.jsonl`, one record per row, so
two nights can be diffed.

**What this box cannot answer, and why.** `p2-kill9` consumes its cluster —
`--chain-epoch 1` is an assertion against an FDB fence that only moves forward
— so it needs a *fresh throwaway* cluster and the script refuses the box's
shared development one by probing it for an `actor/` activation row. The `fdb`
tier wipes key ranges (C-8), so it will not run without
`GATE_STATUS_FDB_IS_THROWAWAY=1` asserting the cluster is disposable. The
cross-platform determinism matrix, `p4-platform-ledger` and the Windows and
macOS accumulation legs need hosted runners this machine does not have. Every
one of those is `SKIPPED` with the reason printed, not quietly absent.

The old full-run timing and disposition report was measured on the retired
shared runner box. It is historical evidence, not a claim about this checkout
or GitHub-hosted CI; re-run the required mode when the answer must be current.

`gate-status.sh --self-test` is part of the `gates` lane. The coverage clause
therefore proves that the report script itself is not an unrun self-test.

**Evidence is read as it is found, and that is a limitation with teeth.**
`--inspect` and the evidence half of `--fast` tell you what a gate last said,
not what it says about `HEAD`: a `target/gates/p1-swarm/` left by a run three commits
ago reads as `PASSED` with that run's numbers. Each JSONL record stamps the
commit of the *report*, not of the evidence. Treat an evidence-derived row as a
citation of an artifact, and re-run `--full` when the answer has to be about
the working tree.

**One gate is restated rather than delegated**, and it is worth knowing:
`determinism-soak` has no script of its own — its body is inline in
`nightly.yml` — so `gate-status.sh` reproduces the ten-repeat loop. A change to
the workflow's version does not reach it. Giving that job a script in
`scripts/` would close the gap.

## Build cache and target directories

Agents work in parallel git worktrees, and a Rust `target/` is enormous. Every
worktree keeps its own `target/`; kache stores compiled objects separately.
Do not turn this into a shared `CARGO_TARGET_DIR`.

Sharing a `CARGO_TARGET_DIR` instead would look tempting and be wrong — cargo
takes an exclusive lock on a target directory, so two agents building at once
would serialize, one waiting on the other for the whole build. The local object
cache has no such contention: identical `rustc` invocations can be reused by
worktrees run by the same user.

### What is configured, and where

| Setting | Location | Committed? |
|---|---|---|
| `build.rustc-wrapper = "kache"` | `.cargo/config.toml` | yes — worktrees each get a copy of tracked files, so this is the only way a setting reaches all of them |
| `build.incremental = false` | `.cargo/config.toml` | yes |
| developer kache store | `~/.cache/kache` | no — local to that Unix user; it has no shared remote on this workstation |
| CI remote | `clippy`'s `kunobi-ninja/kache-action` inputs and its job environment | yes — `s3-bucket: orrery-kache`, `s3-region: eu-central-1`, `s3-prefix: artifacts`, and a 20 GiB local cap |

The standalone tools (`gates/p2-load`, `gates/p3-island`, `p0-*`) each declare their own
`[workspace]`, so each has its own `target/`. They still inherit the repo's
`.cargo/config.toml`, because cargo walks up from the working directory — do
not add a per-tool `.cargo/config.toml`, which would shadow it and silently
drop that tool back to uncached builds.

**Incremental compilation is off deliberately.** An incremental unit is not
cacheable by a plain rustc wrapper, so leaving it on writes artifacts while
defeating the cache for the crate being edited. For a local tight edit loop,
`CARGO_INCREMENTAL=1` overrides the file for that command.

### This makes kache a build prerequisite

If it is missing, install it (see
[kache](https://github.com/kunobi-ninja/kache)) or opt out for one command with
an empty wrapper, which takes precedence over the config file:

```
RUSTC_WRAPPER= cargo build
```

### What exists on the developer box and CI

**`fortyninety` is deliberately local-only.** `.cargo/config.toml` routes
rustc through kache and its store is the invoking user's `~/.cache/kache`, but
the shared S3 bucket is not configured there. As verified on 2026-08-23,
`kache doctor` ends with:

```
All checks passed.
Daemon checks are informational: no remote cache or planner configured (the daemon is optional for local-only use).
```

That is expected, not an instruction to add AWS credentials or a remote to a
developer machine. `./scripts/dev-cache.sh doctor`, `stats`, and `disk` report
the local arrangement; they cannot validate CI's ephemeral S3 credentials.

**The configured machine is the CI `clippy` runner.** For a same-repository
run, that job assumes `orrery-ci-cache` with GitHub OIDC, then
`kunobi-ninja/kache-action` installs kache 0.14.2 and exports the S3 settings.
The 2026-08-21 CI run printed this resolved remote:

```
✓ Remote          s3://orrery-kache/artifacts
✗ Daemon service  not installed
                  → kache daemon install

1 issue(s) found.
```

The daemon-service line is informational on an ephemeral runner; the workflow
asserts the exact `s3://orrery-kache/artifacts` line, rather than treating
`kache doctor`'s exit status as the remote check. Fork pull requests do not
receive an OIDC token and run cold. `fmt`, `gates`, `test`, the determinism
matrix, and nightly jobs also use the workflow-level empty `RUSTC_WRAPPER` or
their own `actions/cache` setup unless a job explicitly installs kache. In
particular, `gates` and `test` have no S3 kache remote by a measured decision,
not an omission.

There are no self-hosted runners: `gh api repos/baadc0de/orrery/actions/runners
--paginate --jq '.total_count'` returned `0` on 2026-08-23. Do not administer
the retired `orrery-hel1-1` runner services or rely on its cache paths.

**Remote retention lives in S3, not on a host.**
`infra/s3.tf` provisions the private `orrery-kache` bucket with all public
access blocked and AES256 SSE-S3, then its
`aws_s3_bucket_lifecycle_configuration.kache` expires the `artifacts/` prefix
at `var.cache_expiration_days`. The current value is 14 days since object
creation, so the bound is `unique bytes uploaded per day × 14`; reads cannot
extend it. A second, bucket-wide rule aborts incomplete multipart uploads one
day after initiation (`var.multipart_abort_days`). There is no bespoke pruning
timer anywhere in this arrangement. Adjusting the retention window means
changing and applying the Terraform variable after measuring upload rate; it
does not mean adding a cron job.

### Working with it

```
./scripts/dev-cache.sh doctor   # is the cache wired up and actually taking effect?
./scripts/dev-cache.sh stats    # hit rate, cache size
./scripts/dev-cache.sh disk     # what every target/ in this checkout costs
./scripts/dev-cache.sh prune    # delete every target/ — safe, and meant to be used
```

`prune` is the lever to pull when disk gets tight: sources are in git and it
removes only this checkout's derived `target/` directories. The developer's
local store may accelerate the rebuild; the CI S3 remote is configured only by
the CI job, so it is not a local fallback.

Two things follow for agents sharing this machine:

- **Prune freely, and prune your own worktree before a long build.** You are not
  destroying anyone's work; you are dropping a derived artifact. The local
  store may accelerate rebuilding it, but this box has no remote fallback.
- **Do not read a less-than-100% hit rate as breakage.** Linking, `build.rs`
  executions and a few binary crate-type units are not cacheable by design, and
  the crate you are actively editing is *supposed* to miss. `kache why-miss
  <crate>` explains any individual miss, which beats guessing.
- **A miss on the crate you are editing is not a miss on its dependencies.**
  The dependency graph is the part kache can pay for, and it is the overwhelming
  majority of a cold build.

## Working alongside other agents

Several agents work this repository at once, each in its own git worktree. The
worktrees isolate the filesystem and nothing else: there is one `.git`, one
developer-local build cache, one disk, one FoundationDB dev cluster, one set of
harness ports, and one GitHub remote. CI's S3 cache is separate infrastructure,
configured only by the CI job; it is not shared state on this workstation.
Everything below exists because of the remaining local asymmetry.

**Be clear about what a collision actually is.** Two agents editing the same
file in two worktrees do *not* clobber each other — separate checkouts, separate
inodes, and neither can see the other's buffer. What they produce is a merge
conflict, discovered later and further from the decision that caused it. That is
worth knowing about in advance, but it is not a reason to stop. The things that
genuinely cannot be shared are elsewhere, and they are the ones worth blocking
on: the `.fdb-dev/` cluster, a harness's fixed ports, `git push` and branch
deletion, `git worktree add/remove`, and the disk itself.

The `.fdb-dev/` cluster is on that list because agents share *one* of it, and on
this machine there is no second one to stand up on a whim.
`scripts/fdb-dev.sh` is written as if there were: `ORRERY_FDB_DEV_PORT`,
`ORRERY_FDB_DEV_DIR`, the cluster description, the memory sizes and the
`FDBSERVER` path all come from the environment, and an instance is identified by
its data directory rather than by its port, so `stop` can never reach an
instance it did not start.

**What exists where, since the 2026-08-22 decision on #176: gates provision
FoundationDB per run and discard it with the runner, and there is no long-running
reference cluster anywhere.** The composite action
[`.github/actions/foundationdb`](.github/actions/foundationdb/action.yml)
installs the client always and, under `server: "true"`, a throwaway single-node
cluster whose server package self-configures `/etc/foundationdb/fdb.cluster`;
that is how `nightly.yml`'s four FDB jobs named above get theirs. What persists
between runs is local convenience only: this workstation (`fortyninety`) keeps a
native `fdbserver` — the `.fdb-dev/` instance described next. The Docker
container that used to be the reference cluster on `orrery-hel1-1` was retired
by that decision; when last checked (2026-08-22) it was still listening there,
awaiting teardown —

```
ssh orrery-hel1-1.distopik.com docker ps -a | grep -i fdb   # still up? a leftover, not a reference
```

— and nothing new may point at it.

**`start` does work here.** An earlier revision of this section said it could
not — that there was no `fdbserver` binary, only `foundationdb-clients`, and
that the process in `ps` was root-owned and lived in a container. All three
claims are wrong, and they were repeated into agent briefings for a day before
anyone checked. Verify for yourself rather than trusting either version:

```
hostname                            # fortyninety — these answers describe THIS box
which fdbserver                     # /usr/bin/fdbserver — the server package IS installed
ss -lntp | grep 4500                # served by fdbserver, not a container
ps -o user= -p <that pid>           # owned by the dev user, not root
docker ps | grep -i fdb             # no fdb container exists
```

So an agent needing a cluster it can clobber should start its **own
`fdbserver`** on a non-default port with its own data directory — which is
exactly what `fdb-dev.sh` is parameterised for — rather than standing up a
container. The shared instance on `127.0.0.1:4500` is still shared: take the
`fdb-dev` lease before writing to it, and never `stop`, `reset` or `pkill` it.

The shared dev cluster serves `127.0.0.1:4500`, with its data and cluster file
under the *main* checkout's `.fdb-dev/`. Tools that look beside their own
checkout — `scripts/fdb-dev.sh`'s `$ROOT/.fdb-dev` default, for one — do not
find it from a worktree, so the route to it is its cluster file:

```
export ORRERY_FDB_CLUSTER_FILE="$(git rev-parse --path-format=absolute --git-common-dir)/../.fdb-dev/fdb.cluster"
fdbcli -C "$ORRERY_FDB_CLUSTER_FILE" --exec 'status minimal'   # dev:test@127.0.0.1:4500
```

Set it explicitly, and do not rely on the fallback: the fdb-gated tests discover
their cluster by walking up from the crate directory looking for any
`.fdb-dev/fdb.cluster`. From a worktree under `.claude/worktrees/` that walk
climbs out of the worktree and lands in the main checkout — an unset variable
does not fail safe here, it quietly aims the run at the shared instance below.
From a checkout with no `.fdb-dev` above it, the other failure mode fires: the
tests `eprintln!("skipping: …")` and pass — green assertions about nothing,
which is exactly the trap `scripts/fdb-tests.sh` exists to close. That script
refuses to default the variable at all.

**Never `stop`, `reset` or `pkill` any of it.** One native `fdbserver` serves
every agent on this box and the tests' default fallback, and it is a *shared
development database*: whatever you write stays. Take the `fdb-dev` lease before
you write to it. If you need a cluster you can clobber, start your own instance
on another port **and in its own directory** — the directory does not follow
the port, and a second instance pointed at the shared data dir is not isolated —

```
ORRERY_FDB_DEV_PORT=4501 ORRERY_FDB_DEV_DIR=/tmp/opencode/fdb-4501 \
  scripts/fdb-dev.sh start        # dev4501:test4501@127.0.0.1:4501, verified 2026-08-22
```

— then point `ORRERY_FDB_CLUSTER_FILE` at `/tmp/opencode/fdb-4501/fdb.cluster`.
An agent running its own instance needs no lease, and should not take one;
`scripts/fdb-dev.sh stop` with the same two variables tears it down and cannot
reach any other instance.

So this arrangement is deliberately two-speed: **lanes are advisory, leases are
exclusive.**

### Where the shared state lives

In the git *common* directory — `$(git rev-parse --path-format=absolute
--git-common-dir)`, which every worktree of this clone resolves to the same
absolute path, and which is never committed. Nowhere else has both properties: a
tracked path is copied per worktree and eventually committed by accident, and a
git-ignored path is per-worktree too.

That last point was a live bug rather than a hypothetical. `.agents/memory/` is
git-ignored, so it existed only in whichever checkout created it and was
invisible from every other worktree — machine-local memory that was really
main-checkout-local memory. It now lives in the common directory, with
`.agents/memory` as a symlink into it. Run `scripts/agent-lane.sh init` once in
a new worktree to create that link.

### The driver

`scripts/agent-lane.sh` is committed, so every worktree gets a copy — the same
reason `.cargo/config.toml` is committed.

```
scripts/agent-lane.sh register --task "..." --paths crates/orrery_witness/,gates/p1-swarm/
scripts/agent-lane.sh list                   # who else is working, on what, where
scripts/agent-lane.sh check <path>...        # does anyone else claim this?
scripts/agent-lane.sh lease acquire fdb-dev  # exclusive; fails if someone holds it
scripts/agent-lane.sh lease release fdb-dev
```

The `fdb-dev` lease is about the *default* instance — the one at `.fdb-dev/` on
port 4500 that every suite falls back to. An agent running its own instance on
its own port needs no lease, and should not take one.

A lane goes stale after 45 minutes without a heartbeat and is reaped
automatically, taking any lease it held with it. A lease that outlives its
holder is the failure mode that makes the next agent wait on nobody, so releases
are not left to good manners.

### What is automatic

`.claude/settings.json` wires four hooks through `scripts/agent-lane-hook.sh`:

| Hook | Does |
|---|---|
| `SessionStart` | registers the lane and injects the current lane table into context |
| `UserPromptSubmit` | heartbeats |
| `PreToolUse` on `Edit`/`Write`/`NotebookEdit` | if another live lane claims the path, asks before proceeding |
| `SessionEnd` | releases the lane and its leases |

The pre-edit hook returns `ask`, never `deny`, for the reason above: the edit is
safe, the merge is the question, and that judgement is not a hook's to make.

Every hook is best-effort and exits zero on any failure. A coordination ledger
that can block work is worse than no ledger.

**What is not automatic is the useful part.** The hook registers a lane with no
task and no paths, which tells a peer nothing. Declare them yourself once you
know what you are doing:

```
scripts/agent-lane.sh register --task "P4: bound witness bandwidth at 32 peers" \
  --paths crates/orrery_witness/,gates/p1-swarm/,docs/03-replication.md
```

### Talking to another agent directly

Sessions on this machine can message each other natively — `ListAgents` to see
them, `SendMessage` to write to one by name. Use it when the ledger is not
enough: you need a decision from whoever holds a lease, you are about to change
an interface they are building against, or their lane says they are somewhere
you are heading.

Prefer the ledger for anything a peer can read at their own pace. A message
interrupts; a lane does not.

### Handing work to a subagent

Within one session, use the `Agent` tool and its worktree isolation rather than
inventing a protocol. Subagents inherit this repository's hooks, so a subagent
that edits into another agent's lane is caught by the same check.

### Codex delegation — live again (2026-08-20)

The weekly quota reset, so routing work to Codex is back on. With opencode
(below) there are now **three** providers with independent quotas, so a wide
fan-out should be **level-loaded** across all of them rather than queued entirely
on one — no single provider's limit is then a hard stop on the whole queue.

Rough division by what each is actually good at here, rather than round-robin:
**Claude** for judgement against an unbuilt design, and for anything that must
commit, push or open a PR; **Codex** for well-specified crate work it can build
and verify, remembering it cannot write to `.git`; **opencode** for read-heavy
investigation and precise citation.

The binary is `codex` (`/usr/bin/codex`). **There is no `cx` wrapper**; earlier
notes naming one are stale. Auth is a ChatGPT account (`codex login status`).

| Model | Use |
|---|---|
| `gpt-5.6-terra` | General coding. Also the configured default in `~/.codex/config.toml`, so a bare `codex exec` already uses it. |
| `gpt-5.6-sol` | Demanding frontier work. |

**Pass the full `gpt-5.6-*` id.** The bare names `terra` and `sol` are rejected —
`The 'terra' model is not supported when using Codex with a ChatGPT account` —
behind a `Model metadata for 'terra' not found` warning that looks like the cause
and is not. The account is fine; the id is wrong.

```
codex exec -m gpt-5.6-sol -s workspace-write -C <dir> "<prompt>"
```

`-s` is `read-only`, `workspace-write` or `danger-full-access`; add `--json` for
JSONL events and `-o <file>` to capture the final message.

**One caveat that matters here: a Codex agent does not inherit this repository's
hooks.** The `SessionStart` lane registration and the `PreToolUse` collision check
in `.claude/settings.json` do not run for it, so it is invisible to
[the lane ledger](#working-alongside-other-agents) unless someone registers it, and
nothing warns it when it edits into another agent's paths. Register its lane on its
behalf, or give it paths that overlap nobody.

### opencode delegation (2026-08-21)

A third provider, alongside Claude and Codex, and free at time of writing. The
binary is `opencode` (`~/.opencode/bin/opencode`, v1.18.20). It needs **no
credentials** — `~/.local/share/opencode/auth.json` is empty and the
`opencode/*-free` models run anyway. `opencode models` lists what is available;
`opencode/x-preview-f-free` is the capable one.

**It is very good at reading code and citing it.** Across four tasks it produced
six `file:line` citations that were checked against the source and every one was
exact, including a correction nobody asked for: `AGENTS.md`'s claim that
`max_size` is silently ignored is true *as a TOML key*, but `KACHE_MAX_SIZE`
does work as an environment variable. Route investigation, grooming and
review-style work to it.

#### The three traps, all of which look like a broken model

Every one of these was hit here before the tool worked, and each cost real time:

```
opencode.jsonc      an explicit permission block, per project
--format json       structured events on stdout
nohup + patience    it does NOT stream; output appears at exit
```

1. **The default renderer needs a TTY.** Redirect it and you get zero bytes.
   Always pass `--format json`, then filter with
   `jq -r 'select(.type=="text")|.part.text'`.
2. **Tool permissions default to asking, and a non-interactive run auto-rejects.**
   The symptom is a silent stall, not an error. `--auto` works but is a blanket
   grant (`auto-approve permissions that are not explicitly denied
   (dangerous!)`); prefer a project `opencode.jsonc` with an explicit block,
   which is scoped and reviewable:

   ```jsonc
   { "$schema": "https://opencode.ai/config.json",
     "permission": { "bash": "allow", "edit": "allow", "read": "allow",
                     "glob": "allow", "grep": "allow", "list": "allow",
                     "lsp": "allow", "task": "allow", "todowrite": "allow",
                     "external_directory": "allow" } }
   ```

   Every key takes `ask`, `allow` or `deny`; the full list is in the schema.
   Note `external_directory` — without it, reading anything outside the project
   (a vendored crate under `~/.cargo/registry`, say) stalls silently.
3. **It buffers output and writes at exit.** A run killed before it finishes
   produces *nothing*, which is indistinguishable from a hang. Multi-step turns
   are slow — a tool call returns quickly but the follow-up model step was
   measured at ~55 s — so a real task runs for many minutes. Launch it detached
   (`nohup … &`) with a generous timeout and read the file afterwards. **Do not
   conclude it has hung because the log is empty.**

That third trap produced three separate wrong diagnoses here, including a
"bootstrap hangs on this repository" conclusion that was false — bootstrap
completes in well under a second, as `--print-logs --log-level DEBUG` shows. The
control that appeared to confirm it (a one-file repo that answered fine) did not
control for elapsed time, which was the variable that actually differed.

#### Reviewing its work

It cannot be trusted more than any other agent, and the same rule applies: **read
the line it cites before repeating the claim.** It has earned that trust on
citations so far; it has not yet been proven on code changes, because every
attempt to test that here was killed by the harness rather than by the model.

### Device-local memory

Durable, machine-local context lives in `.agents/memory/` — a symlink into the
shared store, git-ignored, never committed. Check its `INDEX.md` for notes on
decisions, project state, environment quirks, and open threads. Add or update
entries there (dated, one file per topic) rather than losing context between
sessions. Never store secrets in it.

Notes written there are now read by every agent on this machine, which is the
point, and worth a sentence of care: write what a peer would need, not what you
would need.
