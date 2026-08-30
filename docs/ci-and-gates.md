# CI, the check script, and where every gate stands

> Split out of `AGENTS.md` on 2026-08-30 to keep that file readable. AGENTS.md was 892 lines and every agent loads it.
> It is the same text, relocated; `AGENTS.md` keeps the rules and points here
> for the reasoning, the measurements and the incidents behind them.

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

### The push is the gate

**`./scripts/check.sh` is always run before pushing a branch for a pull
request.** Cutting commits locally without it is fine — the gate is the push,
not the commit, and not the merge.

The only exception is a documentation chore: prose, a plan, an ADR update.
Those need no lane.

This is a rule rather than a habit because two of pull-request CI's checks do
not exist. `static gates` and `workspace tests` moved to nightly on 2026-08-28
to cut roughly twenty minutes of merge latency, and they are the only lanes
that build `clients/regolith` and the standalone gate workspaces. **A pull
request can therefore be green on every required check while `main` does not
compile.** That happened twice on 2026-08-30: once from a targeted
`cargo test -p` standing in for the script (#718, fixed by #719), and once
structurally, when adding a public field to a struct in `crates/orrery_games`
broke a consumer in `clients/regolith` that the writing lane was correctly
scoped away from and could not have seen (#728, fixed by #729).

Read the script's final line. `check: fmt clippy gates test passed` is the
claim; anything else is not.

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

**And `gates` has already outgrown its entry there.** #585 re-measured it on
2026-08-27 over 22 real pull-request runs — still cold and cacheless — at a
mean of **1276 s** (median 1320, range 1004–1380, sd 112). #171's 656 s
describes the 2026-08-20 tree, and the lane has roughly doubled since. The same
issue priced an S3 remote for it and did not wire one; the numbers and the three
reasons are in the `gates:` block of `.github/workflows/ci.yml` and in
[docs/spikes/kache-remote-on-the-gates-lane.md](docs/spikes/kache-remote-on-the-gates-lane.md).

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
cost the matrix its headlessness. Its test suite runs on Linux per commit in
`scripts/check.sh`'s WORKSPACES table (role `test`, run by the `gates` lane).
Release-time launch coverage instead lives in `package-client.yml`'s
three-platform `package-client` matrix (Linux, Windows and macOS), triggered
only by manual dispatches on the allowed ref and `playtest-*` tags; never add a
PR trigger because that workflow will hold the private-assets credential. Each
leg builds the release binary and runs `--render-smoke`: it must remain alive
for twenty seconds and complete a primary-window screenshot before it exits
successfully. This is an assertion that rendering work really completed, not
just that a compile-heavy Bevy process spawned or a green command ran without
doing the client work. Linux supplies Xvfb and forces X11 while removing
`WAYLAND_DISPLAY`; Windows and macOS use the hosted runners' native display
stacks.

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

